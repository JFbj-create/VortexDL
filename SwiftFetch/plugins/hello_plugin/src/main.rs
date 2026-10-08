//! SwiftFetch 示例 EXE 插件
//! 功能: 订阅 evt.chunk_done 事件，每完成 100 个块打印一行

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "v")]
enum IpcMsg {
    #[serde(rename = "1")]
    V1(MessageV1),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum MessageV1 {
    #[serde(rename = "REQ")]
    Req {
        req_id: String,
        method: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    #[serde(rename = "REP")]
    Rep {
        req_id: String,
        status: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    #[serde(rename = "EVT")]
    Evt {
        topic: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    #[serde(rename = "HS1")]
    Handshake {
        protocol: String,
        name: String,
        version: [u32; 3],
    },
    #[serde(rename = "HS_ACK")]
    HandshakeAck {
        host_version: [u32; 3],
        assign_id: u64,
        #[serde(default)]
        feature_flags: u32,
    },
    #[serde(rename = "SHUT")]
    ShutdownV1,
    #[serde(rename = "PING")]
    Ping,
    #[serde(rename = "PONG")]
    Pong,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut pipe_path: Option<String> = None;
    let mut is_plugin = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--sf-plugin" => is_plugin = true,
            "--pipe" => {
                if i + 1 < args.len() {
                    pipe_path = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if !is_plugin || pipe_path.is_none() {
        eprintln!("hello_plugin: 此程序是 SwiftFetch 插件，请勿直接运行");
        eprintln!("用法: hello_plugin.exe --sf-plugin --pipe <named_pipe_path>");
        std::process::exit(1);
    }

    let pipe_path = pipe_path.unwrap();
    run_plugin(&pipe_path).await
}

async fn run_plugin(pipe_path: &str) -> Result<()> {
    let client = loop {
        match ClientOptions::new().open(pipe_path) {
            Ok(c) => break c,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    };

    let (rx, mut tx) = tokio::io::split(client);
    let mut reader = BufReader::new(rx);
    let mut line = String::new();

    let handshake = IpcMsg::V1(MessageV1::Handshake {
        protocol: "swiftfetch-plugin-v1".into(),
        name: "hello_plugin".into(),
        version: [0, 1, 0],
    });
    let hs = serde_json::to_string(&handshake)?;
    tx.write_all(hs.as_bytes()).await?;
    tx.write_all(b"\n").await?;
    tx.flush().await?;

    let mut assigned_id: u64 = 0;
    line.clear();
    if reader.read_line(&mut line).await? > 0 {
        if let Ok(msg) = serde_json::from_str::<IpcMsg>(line.trim()) {
            if let IpcMsg::V1(MessageV1::HandshakeAck { assign_id, .. }) = msg {
                assigned_id = assign_id;
                eprintln!("[hello_plugin] 握手成功, id={}", assigned_id);
            }
        }
    }

    let chunk_count = Arc::new(Mutex::new(0u64));
    let cc = chunk_count.clone();

    loop {
        tokio::select! {
            r = reader.read_line(&mut line) => {
                match r {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() { line.clear(); continue; }
                        if let Ok(msg) = serde_json::from_str::<IpcMsg>(trimmed) {
                            match msg {
                                IpcMsg::V1(MessageV1::Ping) => {
                                    let pong = IpcMsg::V1(MessageV1::Pong);
                                    let s = serde_json::to_string(&pong)?;
                                    tx.write_all(s.as_bytes()).await.ok();
                                    tx.write_all(b"\n").await.ok();
                                    tx.flush().await.ok();
                                }
                                IpcMsg::V1(MessageV1::ShutdownV1) => {
                                    eprintln!("[hello_plugin] 收到 Shutdown, 退出");
                                    break;
                                }
                                IpcMsg::V1(MessageV1::Evt { topic, payload }) => {
                                    if topic == "evt.chunk_done" {
                                        let mut c = cc.lock();
                                        *c += 1;
                                        if *c % 100 == 0 {
                                            println!("[hello_plugin] chunk_done count={} payload={:?}", *c, payload);
                                        }
                                    } else if topic.starts_with("evt.") {
                                        // ignore other events
                                    }
                                }
                                _ => {}
                            }
                        }
                        line.clear();
                    }
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
    }

    Ok(())
}
