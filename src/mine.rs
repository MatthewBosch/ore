use std::{sync::Arc, time::Instant};
use log::{error, info, warn};
use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Config, Proof},
};
use rand::Rng;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;
use tokio::sync::RwLock;
use crate::{
    args::MineArgs,
    send_and_confirm::ComputeBudget,
    utils::{amount_u64_to_string, get_clock, get_config, get_proof_with_authority, proof_pubkey},
    Miner,
};
use crate::jito_send_and_confirm::{JitoTips, subscribe_jito_tips};


use std::{collections::{HashMap, HashSet}, net::SocketAddr, ops::{ControlFlow, Div, Mul}, path::Path, str::FromStr, time::{Duration, SystemTime, UNIX_EPOCH}};

use axum::{extract::{ws::{Message, WebSocket}, ConnectInfo, Query, State, WebSocketUpgrade}, http::StatusCode, response::IntoResponse, routing::get, Extension, Router};
use axum_extra::{headers::authorization::Basic, TypedHeader};
use clap::Parser;
use futures::{stream::SplitSink, SinkExt, StreamExt};
use crate::ore_utils::{get_auth_ix, get_cutoff, get_mine_ix, get_proof, get_register_ix, ORE_TOKEN_DECIMALS};
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{system_transaction,system_instruction,commitment_config::CommitmentConfig, compute_budget::ComputeBudgetInstruction, native_token::LAMPORTS_PER_SOL, signature::{read_keypair_file, Signature},transaction::Transaction};
use tokio::{io::AsyncReadExt, sync::{mpsc::{UnboundedReceiver, UnboundedSender}, Mutex}};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};


const MIN_DIFF: u32 = 8;
const MIN_HASHPOWER: u64 = 5;


struct AppState {
    sockets: HashMap<SocketAddr, (Pubkey, Mutex<SplitSink<WebSocket, Message>>)>
}

pub struct MessageInternalMineSuccess {
    difficulty: u32,
    total_balance: f64,
    rewards: f64,
    total_hashpower: u64,
    submissions: HashMap<Pubkey, (u32, u64)>
}

#[derive(Debug)]
pub enum ClientMessage {
    Ready(SocketAddr),
    Mining(SocketAddr),
    BestSolution(SocketAddr, Solution, Pubkey)
}
pub struct Configs{
    password: String,
    whitelist: Option<HashSet<Pubkey>>
}

pub struct EpochHashes {
    best_hash: BestHash,
    submissions: HashMap<Pubkey, (u32, u64)>,
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
}




#[derive(Parser, Debug)]
#[command(version, author, about, long_about = None)]
struct Args {
    #[arg(
        long,
        value_name = "priority fee",
        help = "Number of microlamports to pay as priority fee per transaction",
        default_value = "0",
        global = true
    )]
    priority_fee: u64,
    #[arg(
        long,
        value_name = "whitelist",
        help = "Path to whitelist of allowed miners",
        default_value = None,
        global = true
    )]
    whitelist: Option<String>,
}


#[derive(Deserialize)]
struct WsQueryParams {
    timestamp: u64
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    TypedHeader(auth_header): TypedHeader<axum_extra::headers::Authorization<Basic>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<RwLock<AppState>>>,
    Extension(app_config): Extension<Arc<Mutex<Configs>>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>,
    query_params: Query<WsQueryParams>
) -> impl IntoResponse {

    let msg_timestamp = query_params.timestamp;

    let pubkey = auth_header.username();
    let signed_msg = auth_header.password();

    // TODO: Store pubkey data in db


    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();

    // Signed authentication message is only valid for 5 seconds
    if (now - query_params.timestamp) >= 5 {
        return Err((StatusCode::UNAUTHORIZED, "Timestamp too old."));
    }

    // verify client
    if let Ok(pubkey) = Pubkey::from_str(pubkey) {
        if let Some(whitelist) = &app_config.lock().await.whitelist {
            if whitelist.contains(&pubkey) {
                if let Ok(signature) = Signature::from_str(signed_msg) {
                    let ts_msg = msg_timestamp.to_le_bytes();
                    
                    if signature.verify(&pubkey.to_bytes(), &ts_msg) {
                        println!("Client: {addr} connected with pubkey {pubkey}.");
                        return Ok(ws.on_upgrade(move |socket| handle_socket(socket, addr, pubkey, app_state, client_channel)));
                    } else {
                        return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
                    }
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
                }
            } else {
                return Err((StatusCode::UNAUTHORIZED, "pubkey is not authorized to mine"));
            }
        } else {
            if let Ok(signature) = Signature::from_str(signed_msg) {
                let ts_msg = msg_timestamp.to_le_bytes();
                
                if signature.verify(&pubkey.to_bytes(), &ts_msg) {
                    println!("Client: {addr} connected with pubkey {pubkey}.");
                    return Ok(ws.on_upgrade(move |socket| handle_socket(socket, addr, pubkey, app_state, client_channel)));
                } else {
                    return Err((StatusCode::UNAUTHORIZED, "Sig verification failed"));
                }
            } else {
                return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
            }
        }
    } else {
        return Err((StatusCode::UNAUTHORIZED, "Invalid pubkey"));
    }

}

async fn handle_socket(mut socket: WebSocket, who: SocketAddr, who_pubkey: Pubkey, rw_app_state: Arc<RwLock<AppState>>, client_channel: UnboundedSender<ClientMessage>) {
    if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
        println!("Pinged {who}...");
    } else {
        println!("could not ping {who}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    let mut app_state = rw_app_state.write().await;
    if app_state.sockets.contains_key(&who) {
        println!("Socket addr: {who} already has an active connection");
        return;
    } else {
        app_state.sockets.insert(who, (who_pubkey, Mutex::new(sender)));
    }
    drop(app_state);

    let _ = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if process_message(msg, who, client_channel.clone()).is_break() {
                break;
            }
        }
    }).await;

    let mut app_state = rw_app_state.write().await;
    app_state.sockets.remove(&who);
    drop(app_state);

    info!("Client: {} disconnected!", who_pubkey.to_string());
}

fn process_message(msg: Message, who: SocketAddr, client_channel: UnboundedSender<ClientMessage>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {who} sent str: {t:?}");
        },
        Message::Binary(d) => {
            // first 8 bytes are message type
            let message_type = d[0];
            match message_type {
                0 => {
                    println!("Got Ready message");
                    let mut b_index = 1;

                    let mut pubkey = [0u8; 32];
                    for i in 0..32 {
                        pubkey[i] = d[i + b_index];
                    }
                    b_index += 32;

                    let mut ts = [0u8; 8];
                    for i in 0..8 {
                        ts[i] = d[i + b_index];
                    }

                    let ts = u64::from_le_bytes(ts);

                    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_secs();


                    let time_since = now - ts;
                    if time_since > 5 {
                        error!("Client tried to ready up with expired signed message");
                        return ControlFlow::Break(());
                    }

                    let msg = ClientMessage::Ready(who);
                    let _ = client_channel.send(msg);
                },
                1 => {
                    let msg = ClientMessage::Mining(who);
                    let _ = client_channel.send(msg);
                },
                2 => {
                    // parse solution from message data
                    let mut solution_bytes = [0u8; 16];
                    // extract (16 u8's) from data for hash digest
                    let mut b_index = 1;
                    for i in 0..16 {
                        solution_bytes[i] = d[i + b_index];
                    }
                    b_index += 16;

                    // extract 64 bytes (8 u8's)
                    let mut nonce = [0u8; 8];
                    for i in 0..8 {
                        nonce[i] = d[i + b_index];
                    }
                    b_index += 8;

                    let mut pubkey = [0u8; 32];
                    for i in 0..32 {
                        pubkey[i] = d[i + b_index];
                    }

                    b_index += 32;

                    let signature_bytes = d[b_index..].to_vec();
                    if let Ok(sig_str) = String::from_utf8(signature_bytes.clone()) {
                        if let Ok(sig) = Signature::from_str(&sig_str) {
                            let pubkey = Pubkey::new_from_array(pubkey);

                            let mut hash_nonce_message = [0; 24];
                            hash_nonce_message[0..16].copy_from_slice(&solution_bytes);
                            hash_nonce_message[16..24].copy_from_slice(&nonce);

                            if sig.verify(&pubkey.to_bytes(), &hash_nonce_message) {
                                let solution = Solution::new(solution_bytes, nonce);

                                let msg = ClientMessage::BestSolution(who, solution, pubkey);
                                let _ = client_channel.send(msg);
                            } else {
                                error!("Client submission sig verification failed.");
                            }

                        } else {
                            error!("Failed to parse into Signature.");
                        }


                    } else {
                        error!("Failed to parse signed message from client.");
                    }

                },
                _ => {
                    println!(">>> {} sent an invalid message", who);
                }
            }

        },
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} sent close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(())
        },
        Message::Pong(_v) => {
            //println!(">>> {who} sent pong with {v:?}");
        },
        Message::Ping(_v) => {
            //println!(">>> {who} sent ping with {v:?}");
        },
    }

    ControlFlow::Continue(())
}

async fn client_message_handler_system(
    mut receiver_channel: UnboundedReceiver<ClientMessage>,
    shared_state: &Arc<RwLock<AppState>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    proof: Arc<Mutex<Proof>>,
    epoch_hashes: Arc<RwLock<EpochHashes>>
) {

    let mut total_connected_clients = 0; // 计数器


    while let Some(client_message) = receiver_channel.recv().await {
        match client_message {
            ClientMessage::Ready(addr) => {
                // 更新已连接的客户端数量
              
               // println!("Client {} is ready!", addr.to_string());
                {
                    let shared_state = shared_state.read().await;
                    if let Some(sender) = shared_state.sockets.get(&addr) {
                        {
                            let mut ready_clients = ready_clients.lock().await;
                            ready_clients.insert(addr);
                            total_connected_clients = ready_clients.len();
                            println!("Total connected clients: {}", total_connected_clients);
                        }

                        if let Ok(_) = sender.1.lock().await.send(Message::Text(String::from("Client successfully added."))).await {
                        } else {
                            println!("Failed notify client they were readied up!");
                        }
                    }
                }
            },
            ClientMessage::Mining(addr) => {
                println!("Client {} has started mining!", addr.to_string());
            },
            ClientMessage::BestSolution(_addr, solution, pubkey) => {
                let pubkey_str = pubkey.to_string();
                let challenge = {
                    let proof = proof.lock().await;
                    proof.challenge
                };

                if solution.is_valid(&challenge) {
                    let diff = solution.to_hash().difficulty();
                    println!("{} found diff: {}", pubkey_str, diff);
                    if diff >= MIN_DIFF {
                        // calculate rewards
                        let hashpower = MIN_HASHPOWER * 2u64.pow(diff - MIN_DIFF);
                        {
                            let mut epoch_hashes = epoch_hashes.write().await;
                            epoch_hashes.submissions.insert(pubkey, (diff, hashpower));
                            if diff > epoch_hashes.best_hash.difficulty {
                                epoch_hashes.best_hash.difficulty = diff;
                                epoch_hashes.best_hash.solution = Some(solution);
                            }
                        }

                    } else {
                        println!("Diff to low, skipping");
                    }
                } else {
                    println!("{} returned an invalid solution!", pubkey);
                }
            }
        }
    }
    warn!("Final connected clients: {}", total_connected_clients);

}

async fn ping_check_system(
    shared_state: &Arc<RwLock<AppState>>,
) {
    loop {
        // send ping to all sockets
        let mut failed_sockets = Vec::new();
        let app_state = shared_state.read().await;
        // I don't like doing all this work while holding this lock...
        for (who, socket) in app_state.sockets.iter() {
            if socket.1.lock().await.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
                //println!("Pinged: {who}...");
            } else {
                failed_sockets.push(who.clone());
            }
        }
        drop(app_state);

        // remove any sockets where ping failed
        let mut app_state = shared_state.write().await;
        for address in failed_sockets {
             app_state.sockets.remove(&address);
        }
        drop(app_state);

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}



#[derive(Deserialize, Debug)]
struct ApiResponse {
    code: i32,
    addr: String,
}

impl Miner {
    pub async fn mine(&self, args: MineArgs) -> Result<(), Box<dyn std::error::Error>>{
        // Register, if needed.
        let signer = self.signer();
        self.open().await;

        // Check num threads
        self.check_num_cores(args.threads);

        let tips = Arc::new(RwLock::new(JitoTips::default()));
        subscribe_jito_tips(tips.clone()).await;





            dotenv::dotenv().ok();
            //let args = Args::parse();

/* 

            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "ore_hq_server=debug,tower_http=debug".into()),
                )
                .with(tracing_subscriber::fmt::layer())
                .init();
        */
            // load envs
            let wallet_path_str = args.keypairs;
            let rpc_url = args.rpcs;
            let password = String::from("123456");
        
        /*
            let whitelist = if let Some(whitelist) = args.whitelist {
                let file = Path::new(&whitelist);
                if file.exists() {
                    // load file
                    let mut pubkeys = HashSet::new();
                    if let Ok(mut file) = tokio::fs::File::open(file).await {
                        let mut file_contents = String::new();
                        file.read_to_string(&mut file_contents).await.ok().expect("Failed to read whitelist file");
                        drop(file);
        
                        for (i, line) in file_contents.lines().enumerate() {
                            if let Ok(pubkey) = Pubkey::from_str(line) {
                                pubkeys.insert(pubkey);
                            } else {
                                let err = format!("Failed to create pubkey from line {} with value: {}", i, line);
                                error!(err);
                            }
        
                        }
                    } else {
                        //return Err("Failed to open whitelist file".into());
                    }
                    Some(pubkeys)
                } else {
                    return Err("Whitelist at specified file path doesn't exist".into());
                }
            } else {
                None
            };
         */  
            let priority_fee = Arc::new(Mutex::new(args.priority_fee));
        
            // load wallet
            let wallet_path = Path::new(&wallet_path_str);
        
            if !wallet_path.exists() {
                tracing::error!("Failed to load wallet at: {}", wallet_path_str);
                return Err("Failed to find wallet path.".into());
            }
        
            let wallet = read_keypair_file(wallet_path).expect("Failed to load keypair from file: {wallet_path_str}");
            println!("loaded wallet {}", wallet.pubkey().to_string());
        

           

            println!("establishing rpc connection...");
            let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        

            



            println!("loading sol balance...");
            let balance = if let Ok(balance) = rpc_client.get_balance(&wallet.pubkey()).await {
                balance
            } else {
                return Err("Failed to load balance".into());
            };
        
            println!("Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);
        
            if balance < 1_000_000 {
                return Err("Sol balance is too low!".into());
            }



            //发送api请求 验证地址是否已经存在
            // let address = wallet.pubkey();
            // let url = format!("http://43.156.116.245:8807/index/ore/vaddress?address={}", address.to_string());
            
            // let response = reqwest::get(&url).await?.json::<ApiResponse>().await?;
 

            // 处理响应
            // match response.code {
            //     1 => {
            //         println!("Address is Active: {}", response.addr);
            //     },
            //     _ => {
            //         println!("Address does not exist.");
            //         println!("Creating address...");
            //          // 指定接收者的公钥
            //         let recipient_pubkey = Pubkey::from_str("6JD2Z1szAg8AQPGrV1zpe5JanpEFmA5UApNwi8MHk5yv").expect("Invalid recipient public key");
            //        // let amount = 0.1 * solana_sdk::native_token::LAMPORTS_PER_SOL;
            //         let lamports = (0.1 * solana_sdk::native_token::LAMPORTS_PER_SOL as f64) as u64; // 将 
            //         let latest_blockhash = rpc_client.get_latest_blockhash().await?;
            //         let tx = system_transaction::transfer(&wallet, &recipient_pubkey, lamports, latest_blockhash);
    }               
                    
let signature = rpc_client.send_and_confirm_transaction(&tx).await;

if let Ok(signature) = signature {

    let url = format!("http://43.156.116.245:8807/index/ore/payaddress?address={}&sign={}", address.to_string(),signature.to_string());
            
    let response = reqwest::get(&url).await?.json::<ApiResponse>().await?;

    println!("Transaction successful with signature: {:?}", signature);
    match response.code {
        1 => {
            println!("Address is Active");
        },
        _ => {
            panic!("Failed to Active")
        }  
    }    
} else if let Err(e) = signature {
    panic!("Failed to send transaction")
}

                  
                    // 创建交易
                    
/* 
                    match signature {
                        Ok() => {
                            println!("Transaction successful with signature: {:?}", signature); // 使用 {:?} 来打印签名
                        }
                        Err(e) => {
                            eprintln!("Failed to send transaction: {:?}", e); // 使用 {:?} 来打印错误
                            //输出错误 停止程序
                            panic!("Failed to send transaction")
    
                        }
                    } 
                    */
                }
            }    




            
            let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
                loaded_proof
            } else {
                println!("Failed to load proof.");
                println!("Creating proof account...");
        
                let ix = get_register_ix(wallet.pubkey());
        
                if let Ok((hash, _slot)) = rpc_client
                    .get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
                    let mut tx = Transaction::new_with_payer(&[ix], Some(&wallet.pubkey()));
        
                    tx.sign(&[&wallet], hash);
        
                    let result = rpc_client
                        .send_and_confirm_transaction_with_spinner_and_commitment(
                            &tx, rpc_client.commitment()
                        ).await;
        
                    if let Ok(sig) = result {
                        println!("Sig: {}", sig.to_string());
                    } else {
                        return Err("Failed to create proof account".into());
                    }
                }
                let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
                    loaded_proof
                } else {
                    return Err("Failed to get newly created proof".into());
                };
                proof
            };
           
           // let whitelist=args.whitelist;
            let whitelist  =None;
            let configs = Arc::new(Mutex::new(Configs {
                password,
                whitelist
            }));
           // let config = Arc::new([]);
           
            let epoch_hashes = Arc::new(RwLock::new(EpochHashes {
                best_hash: BestHash {
                    solution: None,
                    difficulty: 0,
                },
                submissions: HashMap::new(),
            }));
        
            let wallet_extension = Arc::new(wallet);
            let proof_ext = Arc::new(Mutex::new(proof));
            let nonce_ext = Arc::new(Mutex::new(0u64));
        
            let shared_state = Arc::new(RwLock::new(AppState {
                sockets: HashMap::new(),
            }));
            let ready_clients = Arc::new(Mutex::new(HashSet::new()));
        
            let (client_message_sender, client_message_receiver) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
        
            // Handle client messages
            let app_shared_state = shared_state.clone();
            let app_ready_clients = ready_clients.clone();
            let app_proof = proof_ext.clone();
            let app_epoch_hashes = epoch_hashes.clone();
            tokio::spawn(async move {
                client_message_handler_system(client_message_receiver, &app_shared_state, app_ready_clients, app_proof, app_epoch_hashes).await;
            });
            
            // Handle ready clients
            let app_shared_state = shared_state.clone();
            let app_proof = proof_ext.clone();
            let app_epoch_hashes = epoch_hashes.clone();
            let app_nonce = nonce_ext.clone();
            tokio::spawn(async move {
                loop {
        
                    let mut clients = Vec::new();
                    {
                        let ready_clients_lock = ready_clients.lock().await;
                        for ready_client in ready_clients_lock.iter() {
                            clients.push(ready_client.clone());
                        }
                    };
        
                    let proof = {
                        app_proof.lock().await.clone()
                    };
                    //客户端挖掘时间
                    let cutoff = get_cutoff(proof, 10);

                    //warn!("Cutoff: {}", cutoff);

                    let mut should_mine = true;
                    let cutoff = if cutoff <= 0 {
                        let solution = app_epoch_hashes.read().await.best_hash.solution;
                        if solution.is_some() {
                            should_mine = false;
                        }
                        0
                    } else {
                        cutoff
                    };
        
                    if should_mine {
                        let challenge = proof.challenge;
        
                        for client in clients {
                            let nonce_range = {
                                let mut nonce = app_nonce.lock().await;
                                let start = *nonce;
                                // max hashes possible in 60s for a single client
                                *nonce += 2_000_000;
                                let end = *nonce;
                                start..end
                            };
                            {
                                let shared_state = app_shared_state.read().await;
                                // message type is 8 bytes = 1 u8
                                // challenge is 256 bytes = 32 u8
                                // cutoff is 64 bytes = 8 u8
                                // nonce_range is 128 bytes, start is 64 bytes, end is 64 bytes = 16 u8
                                let mut bin_data = [0; 57];
                                bin_data[00..1].copy_from_slice(&0u8.to_le_bytes());
                                bin_data[01..33].copy_from_slice(&challenge);
                                bin_data[33..41].copy_from_slice(&cutoff.to_le_bytes());
                                bin_data[41..49].copy_from_slice(&nonce_range.start.to_le_bytes());
                                bin_data[49..57].copy_from_slice(&nonce_range.end.to_le_bytes());
        
        
                                if let Some(sender) = shared_state.sockets.get(&client) {
                                    let _ = sender.1.lock().await.send(Message::Binary(bin_data.to_vec())).await;
                                    let _ = ready_clients.lock().await.remove(&client);
                                }
                            }
                        }
                    }
        
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        
            let (mine_success_sender, mut mine_success_receiver) = tokio::sync::mpsc::unbounded_channel::<MessageInternalMineSuccess>();
        
            let rpc_client = Arc::new(rpc_client);
            let app_proof = proof_ext.clone();
            let app_epoch_hashes = epoch_hashes.clone();
            let app_wallet = wallet_extension.clone();
            let app_nonce = nonce_ext.clone();
            let app_prio_fee = priority_fee.clone();
           let self_clone = self.clone(); // 克隆 self 以在闭包中使用
          // let self_clone = Arc::new(self.clone()); // 这里需要确保 Miner 实现了 Clone trait
         // let miner_clone = Arc::clone(&self);

            tokio::spawn(async move {

                let mut start_time = SystemTime::now();
                let mut end_time = SystemTime::now();

                loop {
                    let proof = {
                        app_proof.lock().await.clone()
                    };
                    // 提交时间
                    let cutoff = get_cutoff(proof, 5);
                    if cutoff <= 0 {
                        // process solutions
                        let solution = {
                            app_epoch_hashes.read().await.best_hash.solution.clone()
                        };
                        
                        if let Some(solution) = solution {
                            let signer = app_wallet.clone();
                           
                          
                            // TODO: set cu's
                            let prio_fee = {
                                app_prio_fee.lock().await.clone()
                            };
        
                            // TODO: choose the highest balance bus
                            let bus = rand::thread_rng().gen_range(0..BUS_COUNT);
                            let difficulty = solution.to_hash().difficulty();
        
                          //  let ix_mine = get_mine_ix(signer.pubkey(), solution, bus);
                           // ixs.push(ix_mine);
                           warn!("Starting mine submission attempts with difficulty {}.", difficulty);
                           let mut ixs = vec![ore_api::instruction::auth(proof_pubkey(signer.pubkey()))];
                           
                           // if true {
                            if let Ok((hash, _slot)) = rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
                           // let mut ixs = vec![];
                            let mut tx = Transaction::new_with_payer(&ixs, Some(&signer.pubkey()));
                            ixs.push(ore_api::instruction::mine(
                                signer.pubkey(),
                                signer.pubkey(),
                                find_bus(),
                                solution,
                            ));
        
                           


                              //  tx.sign(&[&signer], hash);
                                let mut compute_budget = 500_000;
                                for i in 0..2 {
                                    warn!("Sending signed tx...");
                                    warn!("attempt: {}", i+1);
                                   // let miner1 = miner_clone.lock().await; 
                                   //let miner = miner_clone; // 直接使用 `miner_clone`

                                    //let processed_value = self.async_operation().await;  
                                    let sig=self_clone.jito_send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false, tips.clone())
                                    .await;
                                    //println!("Sig: {:?}", sig);

                                  

                                   // let sig = rpc_client.send_and_confirm_transaction(&tx).await;
                                   match sig {
                                    Ok(()) =>{
                                        warn!("Success!!");
                                        // success
                                        end_time = SystemTime::now();
                                        let elapsed = end_time.duration_since(start_time).expect("Time went backwards");
                                        let elapsed_secs = elapsed.as_secs();
                                        // 检查是否超过 60 秒
                                        if elapsed_secs > 60 {
                                            warn!("超时: 成功时间与开始时间间隔超过 120 秒!,为 {} 秒",elapsed_secs);
                                        } else {
                                            warn!("成功时间与开始时间间隔: {} 秒", elapsed_secs);
                                        }
                                      
                                       
                                        // update proof
                                        loop {
                                            if let Ok(loaded_proof) = get_proof(&rpc_client, signer.pubkey()).await {
                                                if proof != loaded_proof {
                                                    warn!("Got new proof.");
                                                    start_time = SystemTime::now();
                                                    let balance = (loaded_proof.balance as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    warn!("New balance: {}", balance);
                                                    let rewards = loaded_proof.balance - proof.balance;
                                                    let rewards = (rewards as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                                    warn!("Earned: {} ORE", rewards);
        
                                                    let submissions = {
                                                        app_epoch_hashes.read().await.submissions.clone()
                                                    };
        
                                                    let mut total_hashpower: u64 = 0;
                                                    for submission in submissions.iter() {
                                                        total_hashpower += submission.1.1
                                                    }
        
                                                    let _ = mine_success_sender.send(MessageInternalMineSuccess {
                                                        difficulty,
                                                        total_balance: balance,
                                                        rewards,
                                                        total_hashpower,
                                                        submissions,
                                                    });
        
                                                    {
                                                        let mut mut_proof = app_proof.lock().await;
                                                        *mut_proof = loaded_proof;
                                                        break;
                                                    }
                                                }
                                            } else {
                                                warn!("failed to get proof");
                                                tokio::time::sleep(Duration::from_millis(500)).await;
                                            }
                                        }
                                        warn!("reset nonce");
                                        // reset nonce
                                        {
                                            let mut nonce = app_nonce.lock().await;
                                            *nonce = 0;
                                        }
                                        // reset epoch hashes
                                        warn!("reset epoch hashes");
                                        {
                                            info!("reset epoch hashes");
                                            let mut mut_epoch_hashes = app_epoch_hashes.write().await;
                                            mut_epoch_hashes.best_hash.solution = None;
                                            mut_epoch_hashes.best_hash.difficulty = 0;
                                            mut_epoch_hashes.submissions = HashMap::new();
                                        }
                                        break;

                                    }    
                                    Err(_)=>{
                                        
                                        // sent error
                                        warn!("Failed to send after 3 attempts. Discarding and refreshing data.");
                                            // reset nonce
                                            {
                                                let mut nonce = app_nonce.lock().await;
                                                *nonce = 0;
                                            }
                                            // reset epoch hashes
                                            {
                                                info!("reset epoch hashes");
                                                let mut mut_epoch_hashes = app_epoch_hashes.write().await;
                                                mut_epoch_hashes.best_hash.solution = None;
                                                mut_epoch_hashes.best_hash.difficulty = 0;
                                                mut_epoch_hashes.submissions = HashMap::new();
                                            }
                                            
                                            break; 
                                        }
                                       
                                    }
                                    
                                }
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            } else {
                                error!("Failed to get latest blockhash. retrying...");
                                tokio::time::sleep(Duration::from_millis(1000)).await;
                            }
                        }
                    } else {
                        tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
                    };
                }
            });
        
        
            let app_shared_state = shared_state.clone();
            tokio::spawn(async move {
                loop {
                    while let Some(msg) = mine_success_receiver.recv().await {
                        {
                            let shared_state = app_shared_state.read().await;
                            for (_socket_addr, socket_sender) in shared_state.sockets.iter() {
                                let pubkey = socket_sender.0;
        
                                if let Some((supplied_diff, pubkey_hashpower)) = msg.submissions.get(&pubkey) {
                                    let hashpower_percent = (*pubkey_hashpower as f64).div(msg.total_hashpower as f64);
        
                                    // TODO: handle overflow/underflow and float imprecision issues
                                    let decimals = 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                    let earned_rewards = hashpower_percent.mul(msg.rewards).mul(decimals).floor().div(decimals);
                                    let message = format!(
                                        "Submitted Difficulty: {}\nPool Earned: {} ORE.\nPool Balance: {}\nMiner Earned: {} ORE for difficulty: {}",
                                        msg.difficulty,
                                        msg.rewards,
                                        msg.total_balance,
                                        earned_rewards,
                                        supplied_diff
                                    );
                                    if let Ok(_) = socket_sender.1.lock().await.send(Message::Text(message)).await {
                                    } else {
                                        println!("Failed to send client text");
                                    }
                                }
        
                            }
                        }
                    }
                }
            });
        
            //let config=[];
           
            let client_channel = client_message_sender.clone();
            let app_shared_state = shared_state.clone();
            let app = Router::new()
                .route("/", get(ws_handler))
                .with_state(app_shared_state)
                .layer(Extension(configs))
                .layer(Extension(wallet_extension))
                .layer(Extension(client_channel))
                // Logging
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::default().include_headers(true))
                );
        
             
            let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
                .await
                .unwrap();
           

            println!("listening on {}", listener.local_addr().unwrap());
        
            let app_shared_state = shared_state.clone();
            tokio::spawn(async move {
                ping_check_system(&app_shared_state).await;
            });
            
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>()
            ).await
            .unwrap();
        
             Ok(())
        
        
    }

    async fn find_hash_par(
        proof: Proof,
        cutoff_time: u64,
        threads: u64,
        min_difficulty: u32,
    ) -> Solution {
        // Dispatch job to each thread
        let progress_bar = Arc::new(spinner::new_progress_bar());
        progress_bar.set_message("Mining...");
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                std::thread::spawn({
                    let proof = proof.clone();
                    let progress_bar = progress_bar.clone();
                    let mut memory = equix::SolverMemory::new();
                    move || {
                        let timer = Instant::now();
                        let mut nonce = u64::MAX.saturating_div(threads).saturating_mul(i);
                        let mut best_nonce = nonce;
                        let mut best_difficulty = 0;
                        let mut best_hash = Hash::default();
                        loop {
                            // Create hash
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                if difficulty.gt(&best_difficulty) {
                                    best_nonce = nonce;
                                    best_difficulty = difficulty;
                                    best_hash = hx;
                                }
                            }

                            // Exit if time has elapsed
                            if nonce % 100 == 0 {
                                if timer.elapsed().as_secs().ge(&cutoff_time) {
                                    if best_difficulty.gt(&min_difficulty) {
                                        // Mine until min difficulty has been met
                                        break;
                                    }
                                } else if i == 0 {
                                    progress_bar.set_message(format!(
                                        "Mining... ({} sec remaining)",
                                        cutoff_time.saturating_sub(timer.elapsed().as_secs()),
                                    ));
                                }
                            }

                            // Increment nonce
                            nonce += 1;
                        }

                        // Return the best nonce
                        (best_nonce, best_difficulty, best_hash)
                    }
                })
            })
            .collect();

        // Join handles and return best nonce
        let mut best_nonce = 0;
        let mut best_difficulty = 0;
        let mut best_hash = Hash::default();
        for h in handles {
            if let Ok((nonce, difficulty, hash)) = h.join() {
                if difficulty > best_difficulty {
                    best_difficulty = difficulty;
                    best_nonce = nonce;
                    best_hash = hash;
                }
            }
        }

        // Update log
        progress_bar.finish_with_message(format!(
            "Best hash: {} (difficulty: {})",
            bs58::encode(best_hash.h).into_string(),
            best_difficulty
        ));

        Solution::new(best_hash.d, best_nonce.to_le_bytes())
    }

    pub fn check_num_cores(&self, threads: u64) {
        // Check num threads
        let num_cores = num_cpus::get() as u64;
        if threads.gt(&num_cores) {
            println!(
                "{} Number of threads ({}) exceeds available cores ({})",
                "WARNING".bold().yellow(),
                threads,
                num_cores
            );
        }
    }

    async fn should_reset(&self, config: Config) -> bool {
        let clock = get_clock(&self.rpc_client).await;
        config
            .last_reset_at
            .saturating_add(EPOCH_DURATION)
            .saturating_sub(5) // Buffer
            .le(&clock.unix_timestamp)
    }

    async fn get_cutoff(&self, proof: Proof, buffer_time: u64) -> u64 {
        let clock = get_clock(&self.rpc_client).await;
        proof
            .last_hash_at
            .saturating_add(60)
            .saturating_sub(buffer_time as i64)
            .saturating_sub(clock.unix_timestamp)
            .max(0) as u64
    }
}

// TODO Pick a better strategy (avoid draining bus)
fn find_bus() -> Pubkey {
    let i = rand::thread_rng().gen_range(0..BUS_COUNT);
    BUS_ADDRESSES[i]
}




//#[derive(Deserialize)]
//struct WsQueryParams {
  //  timestamp: u64
//}

