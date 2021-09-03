use crate::blockchain::{rocks::columns, Rocks, RocksColumn, Slab};
use crate::cli::{TransferParams, WithdrawParams};
use crate::crypto::{
    load_params,
    merkle::{CommitmentTree, IncrementalWitness},
    merkle_node::MerkleNode,
    note::{EncryptedNote, Note},
    nullifier::Nullifier,
    save_params, setup_mint_prover, setup_spend_prover,
};
use crate::rpc::jsonserver;
use crate::serial::Encodable;
use crate::serial::{deserialize, serialize, Decodable};
use crate::service::{CashierClient, GatewayClient, GatewaySlabsSubscriber};
use crate::state::{state_transition, ProgramState, StateUpdate};
use crate::wallet::WalletPtr;
use crate::{tx, Error, Result};

use super::ClientFailed;

use async_executor::Executor;
use bellman::groth16;
use bls12_381::Bls12;
use log::*;
use rusqlite::Connection;

use jsonrpc_core::{BoxFuture, IoHandler};
use jsonrpc_derive::rpc;

use async_std::sync::{Arc, Mutex};
use futures::FutureExt;
use std::net::SocketAddr;
use std::path::PathBuf;

pub struct Client {
    state: State,
    secret: jubjub::Fr,
    mint_params: bellman::groth16::Parameters<Bls12>,
    spend_params: bellman::groth16::Parameters<Bls12>,
    gateway: GatewayClient,
}

impl Client {
    pub fn new(
        secret: jubjub::Fr,
        rocks: Arc<Rocks>,
        gateway_addrs: (SocketAddr, SocketAddr),
        params_paths: (PathBuf, PathBuf),
        wallet_path: PathBuf,
    ) -> Result<Self> {
        let slabstore = RocksColumn::<columns::Slabs>::new(rocks.clone());
        let merkle_roots = RocksColumn::<columns::MerkleRoots>::new(rocks.clone());
        let nullifiers = RocksColumn::<columns::Nullifiers>::new(rocks);

        let mint_params_path = params_paths.0.to_str().unwrap_or("mint.params");
        let spend_params_path = params_paths.1.to_str().unwrap_or("spend.params");

        // Auto create trusted ceremony parameters if they don't exist
        if !params_paths.0.exists() {
            let params = setup_mint_prover();
            save_params(mint_params_path, &params)?;
        }
        if !params_paths.1.exists() {
            let params = setup_spend_prover();
            save_params(spend_params_path, &params)?;
        }

        // Load trusted setup parameters
        let (mint_params, mint_pvk) = load_params(mint_params_path)?;
        let (spend_params, spend_pvk) = load_params(spend_params_path)?;

        let state = State {
            tree: CommitmentTree::empty(),
            merkle_roots,
            nullifiers,
            mint_pvk,
            spend_pvk,
            wallet_path,
        };

        // create gateway client
        debug!(target: "CLIENT", "Creating GatewayClient");
        let gateway = GatewayClient::new(gateway_addrs.0, gateway_addrs.1, slabstore)?;

        Ok(Self {
            state,
            secret,
            mint_params,
            spend_params,
            gateway,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        self.gateway.start().await?;
        Ok(())
    }

    pub async fn connect_to_cashier(
        client: Client,
        executor: Arc<Executor<'_>>,
        wallet: WalletPtr,
        cashier_addr: SocketAddr,
        rpc_url: SocketAddr,
    ) -> Result<()> {
        // create cashier client
        debug!(target: "CLIENT", "Creating cashier client");
        let mut cashier_client = CashierClient::new(cashier_addr)?;

        // start cashier_client
        cashier_client.start().await?;

        let client_mutex = Arc::new(Mutex::new(client));
        let cashier_mutex = Arc::new(Mutex::new(cashier_client));

        let mut io = IoHandler::new();
        let rpcimpl = RpcUserAdapter {
            wallet: wallet.clone(),
            client: client_mutex.clone(),
            cashier_client: cashier_mutex.clone(),
        };

        io.extend_with(rpcimpl.to_delegate());

        let io = Arc::new(io);

        // start the rpc server
        debug!(target: "CLIENT", "Start RPC server");
        let _ = jsonserver::start(executor.clone(), rpc_url, io).await?;

        // start subscriber
        Client::connect_to_subscriber(client_mutex.clone(), executor.clone(), wallet.clone())
            .await?;

        Ok(())
    }

    pub async fn transfer(
        self: &mut Client,
        transfer_params: TransferParams,
        wallet: WalletPtr,
    ) -> Result<()> {
        let pub_key = transfer_params.pub_key;

        let address = bs58::decode(pub_key.clone())
            .into_vec()
            .map_err(|_| ClientFailed::UnvalidAddress(pub_key.clone()))?;

        let address: jubjub::SubgroupPoint =
            deserialize(&address).map_err(|_| ClientFailed::UnvalidAddress(pub_key))?;

        let amount = transfer_params.amount;

        if amount <= 0.0 {
            return Err(ClientFailed::UnvalidAmount(amount as u64).into());
        }

        // check if there are coins
        let own_coins = wallet.get_own_coins()?;

        if own_coins.is_empty() {
            return Err(ClientFailed::NotEnoughValue(0).into());
        }

        let witness = &own_coins[0].3;
        let merkle_path = witness.path().unwrap();

        // Construct a new tx spending the coin
        let builder = tx::TransactionBuilder {
            clear_inputs: vec![],
            inputs: vec![tx::TransactionBuilderInputInfo {
                merkle_path,
                secret: self.secret.clone(),
                note: own_coins[0].1.clone(),
            }],
            // We can add more outputs to this list.
            // The only constraint is that sum(value in) == sum(value out)
            outputs: vec![tx::TransactionBuilderOutputInfo {
                value: amount as u64,
                asset_id: 1,
                public: address,
            }],
        };
        // Build the tx
        let mut tx_data = vec![];
        {
            let tx = builder.build(&self.mint_params, &self.spend_params);
            tx.encode(&mut tx_data).expect("encode tx");
        }

        // build slab from the transaction
        let slab = Slab::new(tx_data);

        self.gateway.put_slab(slab).await?;

        Ok(())
    }

    pub async fn connect_to_subscriber(
        client: Arc<Mutex<Client>>,
        executor: Arc<Executor<'_>>,
        wallet: WalletPtr,
    ) -> Result<()> {
        // start subscribing
        debug!(target: "CLIENT", "Start subscriber");
        let gateway_slabs_sub: GatewaySlabsSubscriber = client
            .lock()
            .await
            .gateway
            .start_subscriber(executor.clone())
            .await?;

        loop {
            let slab = gateway_slabs_sub.recv().await?;
            let tx = tx::Transaction::decode(&slab.get_payload()[..])?;
            let mut client = client.lock().await;
            let update = state_transition(&client.state, tx)?;
            client.state.apply(update, wallet.clone()).await?;
        }
    }
}

pub struct State {
    // The entire merkle tree state
    pub tree: CommitmentTree<MerkleNode>,
    // List of all previous and the current merkle roots
    // This is the hashed value of all the children.
    pub merkle_roots: RocksColumn<columns::MerkleRoots>,
    // Nullifiers prevent double spending
    pub nullifiers: RocksColumn<columns::Nullifiers>,
    // Mint verifying key used by ZK
    pub mint_pvk: groth16::PreparedVerifyingKey<Bls12>,
    // Spend verifying key used by ZK
    pub spend_pvk: groth16::PreparedVerifyingKey<Bls12>,
    // TODO: remove this
    wallet_path: PathBuf,
}

impl ProgramState for State {
    fn is_valid_cashier_public_key(&self, _public: &jubjub::SubgroupPoint) -> bool {
        // TODO: use walletdb instead of connecting with sqlite directly
        let conn = Connection::open(self.wallet_path.clone()).expect("Connect to database");
        let mut stmt = conn
            .prepare("SELECT key_public FROM cashier WHERE key_public IN (SELECT key_public)")
            .expect("Generate statement");
        stmt.exists([1i32]).expect("Read database")
        // do actual validity check
    }

    fn is_valid_merkle(&self, merkle_root: &MerkleNode) -> bool {
        self.merkle_roots
            .key_exist(*merkle_root)
            .expect("Check if the merkle_root valid")
    }

    fn nullifier_exists(&self, nullifier: &Nullifier) -> bool {
        self.nullifiers
            .key_exist(nullifier.repr)
            .expect("Check if nullifier exists")
    }

    // load from disk
    fn mint_pvk(&self) -> &groth16::PreparedVerifyingKey<Bls12> {
        &self.mint_pvk
    }

    fn spend_pvk(&self) -> &groth16::PreparedVerifyingKey<Bls12> {
        &self.spend_pvk
    }
}

impl State {
    pub async fn apply(&mut self, update: StateUpdate, wallet: WalletPtr) -> Result<()> {
        // Extend our list of nullifiers with the ones from the update
        for nullifier in update.nullifiers {
            self.nullifiers.put(nullifier, vec![] as Vec<u8>)?;
        }

        // Update merkle tree and witnesses
        for (coin, enc_note) in update.coins.into_iter().zip(update.enc_notes.into_iter()) {
            // Add the new coins to the merkle tree
            let node = MerkleNode::from_coin(&coin);
            self.tree.append(node).expect("Append to merkle tree");

            // Keep track of all merkle roots that have existed
            self.merkle_roots.put(self.tree.root(), vec![] as Vec<u8>)?;

            // Also update all the coin witnesses
            for witness in wallet.witnesses.lock().await.iter_mut() {
                witness.append(node).expect("Append to witness");
            }

            if let Some((note, secret)) = self.try_decrypt_note(wallet.clone(), enc_note).await {
                // We need to keep track of the witness for this coin.
                // This allows us to prove inclusion of the coin in the merkle tree with ZK.
                // Just as we update the merkle tree with every new coin, so we do the same with
                // the witness.

                // Derive the current witness from the current tree.
                // This is done right after we add our coin to the tree (but before any other
                // coins are added)

                // Make a new witness for this coin
                let witness = IncrementalWitness::from_tree(&self.tree);

                wallet.put_own_coins(coin.clone(), note.clone(), witness.clone(), secret)?;
            }
        }
        Ok(())
    }

    async fn try_decrypt_note(
        &self,
        wallet: WalletPtr,
        ciphertext: EncryptedNote,
    ) -> Option<(Note, jubjub::Fr)> {
        let secret = wallet.get_private().ok()?;
        match ciphertext.decrypt(&secret) {
            Ok(note) => {
                // ... and return the decrypted note for this coin.
                return Some((note, secret.clone()));
            }
            Err(_) => {}
        }
        // We weren't able to decrypt the note with our key.
        None
    }
}

/// Rpc trait
#[rpc(server)]
pub trait Rpc {
    /// Adds two numbers and returns a result
    #[rpc(name = "say_hello")]
    fn say_hello(&self) -> Result<String>;

    /// get key
    #[rpc(name = "get_key")]
    fn get_key(&self) -> Result<String>;

    /// create_wallet
    #[rpc(name = "create_wallet")]
    fn create_wallet(&self) -> Result<String>;

    /// key_gen
    #[rpc(name = "key_gen")]
    fn key_gen(&self) -> Result<String>;

    /// transfer
    #[rpc(name = "transfer")]
    fn transfer(&self, pub_key: String, amount: f64) -> BoxFuture<Result<String>>;

    /// withdraw
    #[rpc(name = "withdraw")]
    fn withdraw(&self, pub_key: String, amount: f64) -> BoxFuture<Result<String>>;

    /// deposit
    #[rpc(name = "deposit")]
    fn deposit(&self) -> BoxFuture<Result<String>>;
}

struct RpcUserAdapter {
    wallet: WalletPtr,
    client: Arc<Mutex<Client>>,
    cashier_client: Arc<Mutex<CashierClient>>,
}

impl RpcUserAdapter {
    async fn transfer_process(
        client: Arc<Mutex<Client>>,
        wallet: WalletPtr,
        transfer_params: TransferParams,
    ) -> Result<String> {
        let address = transfer_params.pub_key.clone();
        let amount = transfer_params.amount.clone();

        client
            .lock()
            .await
            .transfer(transfer_params, wallet.clone())
            .await?;

        Ok(format!("transfered {} DRK to {}", amount, address))
    }

    async fn withdraw_process(
        client: Arc<Mutex<Client>>,
        cashier_client: Arc<Mutex<CashierClient>>,
        wallet: WalletPtr,
        withdraw_params: WithdrawParams,
    ) -> Result<String> {
        let address = withdraw_params.pub_key.clone();
        let amount = withdraw_params.amount.clone();

        let drk_public = cashier_client
            .lock()
            .await
            .withdraw(address)
            .await
            .map_err(|err| ClientFailed::from(err))?;

        if let Some(drk_addr) = drk_public {
            let drk_addr = bs58::encode(serialize(&drk_addr)).into_string();

            client
                .lock()
                .await
                .transfer(
                    TransferParams {
                        pub_key: drk_addr.clone(),
                        amount,
                    },
                    wallet.clone(),
                )
                .await?;

            return Ok(format!(
                "sending {} dbtc to provided address for withdrawing: {} ",
                amount, drk_addr
            ));
        } else {
            return Err(Error::from(ClientFailed::UnableToGetWithdrawAddress));
        }
    }

    async fn deposit_process(
        cashier_client: Arc<Mutex<CashierClient>>,
        wallet: WalletPtr,
    ) -> Result<String> {
        let deposit_addr = wallet.get_public()?;
        let btc_public = cashier_client
            .lock()
            .await
            .get_address(deposit_addr)
            .await
            .map_err(|err| ClientFailed::from(err))?;

        if let Some(btc_addr) = btc_public {
            return Ok(btc_addr.to_string());
        } else {
            return Err(Error::from(ClientFailed::UnableToGetDepositAddress));
        }
    }
}

impl Rpc for RpcUserAdapter {
    fn say_hello(&self) -> Result<String> {
        debug!(target: "RPC USER ADAPTER", "say_hello() [START]");
        Ok(String::from("hello world"))
    }

    fn get_key(&self) -> Result<String> {
        debug!(target: "RPC USER ADAPTER", "get_key() [START]");
        let key_public = self.wallet.get_public()?;
        let bs58_address = bs58::encode(serialize(&key_public)).into_string();
        Ok(bs58_address)
    }

    fn create_wallet(&self) -> Result<String> {
        debug!(target: "RPC USER ADAPTER", "create_wallet() [START]");
        self.wallet.init_db()?;
        Ok("wallet creation successful".into())
    }

    fn key_gen(&self) -> Result<String> {
        debug!(target: "RPC USER ADAPTER", "key_gen() [START]");
        let (public, private) = self.wallet.key_gen();
        debug!(target: "RPC USER ADAPTER", "Created keypair...");
        debug!(target: "RPC USER ADAPTER", "Attempting to write to database...");
        self.wallet.put_keypair(public, private)?;
        Ok("key generation successful".into())
    }

    fn transfer(&self, pub_key: String, amount: f64) -> BoxFuture<Result<String>> {
        debug!(target: "RPC USER ADAPTER", "transfer() [START]");
        let transfer_params = TransferParams { pub_key, amount };
        Self::transfer_process(self.client.clone(), self.wallet.clone(), transfer_params).boxed()
    }

    fn withdraw(&self, pub_key: String, amount: f64) -> BoxFuture<Result<String>> {
        debug!(target: "RPC USER ADAPTER", "withdraw() [START]");
        let withdraw_params = WithdrawParams { pub_key, amount };
        Self::withdraw_process(
            self.client.clone(),
            self.cashier_client.clone(),
            self.wallet.clone(),
            withdraw_params,
        )
        .boxed()
    }

    fn deposit(&self) -> BoxFuture<Result<String>> {
        debug!(target: "RPC USER ADAPTER", "deposit() [START]");
        Self::deposit_process(self.cashier_client.clone(), self.wallet.clone()).boxed()
    }
}
