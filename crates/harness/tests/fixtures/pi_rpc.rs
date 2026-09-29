//! Stateful cross-platform Pi RPC peer; never built into the application.
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}
fn response(v: &Value, data: Value) {
    emit(json!({"type":"response","id":v["id"],"command":v["type"],"success":true,"data":data}));
}
fn message(text: &str, stop: &str) {
    emit(
        json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":text}],"stopReason":stop,"errorMessage":"mock provider failure","usage":{"input":12,"output":3}}}),
    );
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("0.85.1");
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        std::process::exit(99);
    });
    let file = args
        .iter()
        .position(|a| a == "--session")
        .map(|i| args[i + 1].clone())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap()
                .join("fixture-session.jsonl")
                .display()
                .to_string()
        });
    let session = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.lines().next()?).ok())
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "pi-fixture-session".into());
    if !args.iter().any(|a| a == "--no-session") && !std::path::Path::new(&file).exists() {
        std::fs::write(
            &file,
            format!(
                "{}\n",
                json!({"type":"session","id":session,"cwd":std::env::current_dir().unwrap()})
            ),
        )
        .unwrap();
    }
    let active = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let mut dialog = None;
    let mut model = "mock".to_string();
    let mut thinking = "medium".to_string();
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match v["type"].as_str().unwrap_or("") {
            "get_state" => response(
                &v,
                json!({"sessionId":session,"sessionFile":file,"isStreaming":active.load(Ordering::SeqCst),"isCompacting":false,"model":{"id":model,"provider":"mock","contextWindow":128000},"thinkingLevel":thinking}),
            ),
            "get_available_models" => response(
                &v,
                json!({"models":[{"id":"mock","provider":"mock","name":"Mock","reasoning":true,"contextWindow":128000}]}),
            ),
            "get_available_thinking_levels" => {
                response(&v, json!({"levels":["off","low","medium","high"]}))
            }
            "set_model" => {
                model = v["modelId"].as_str().unwrap().into();
                response(&v, json!({"id":model}));
            }
            "set_thinking_level" => {
                thinking = v["level"].as_str().unwrap().into();
                response(&v, json!({}));
            }
            "get_commands" => response(
                &v,
                json!({"commands":[{"name":"noop","description":"handled","source":"extension"},{"name":"skill:probe","description":"skill","source":"skill"}]}),
            ),
            "prompt" => {
                let text = v["message"].as_str().unwrap_or("").to_owned();
                if text == "reject" {
                    emit(
                        json!({"type":"response","id":v["id"],"command":"prompt","success":false,"error":"preflight rejected"}),
                    );
                    continue;
                }
                if text == "/noop" || text == "handled" {
                    response(&v, json!({}));
                    continue;
                }
                if text.starts_with("/dialog") {
                    dialog = Some(v.clone());
                    emit(
                        json!({"type":"extension_ui_request","id":"question","method":text.split_whitespace().nth(1).unwrap_or("input"),"title":"Choose","options":["first","second"],"prefill":"initial\nvalue"}),
                    );
                    continue;
                }
                if text == "crash" {
                    eprintln!("mock crash diagnostic");
                    std::process::exit(7);
                }
                active.store(true, Ordering::SeqCst);
                abort.store(false, Ordering::SeqCst);
                response(&v, json!({}));
                emit(json!({"type":"agent_start"}));
                emit(
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":text}]}}),
                );
                let active = active.clone();
                let abort = abort.clone();
                std::thread::spawn(move || {
                    if text == "slow" {
                        for _ in 0..100 {
                            if abort.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    if text == "retry" {
                        message("", "error");
                        emit(json!({"type":"agent_end","willRetry":true}));
                        std::thread::sleep(Duration::from_millis(150));
                        emit(json!({"type":"agent_start"}));
                    }
                    if text == "compact" {
                        emit(json!({"type":"agent_end"}));
                        emit(json!({"type":"compaction_start"}));
                        std::thread::sleep(Duration::from_millis(100));
                        emit(json!({"type":"compaction_end","result":{"estimatedTokensAfter":9}}));
                    }
                    if abort.load(Ordering::SeqCst) {
                        message("", "aborted");
                    } else if text == "error" {
                        message("", "error");
                    } else {
                        emit(json!({"type":"message_start","message":{"role":"assistant"}}));
                        emit(
                            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"reply:"}}),
                        );
                        message(&format!("reply:{text}"), "stop");
                    }
                    emit(json!({"type":"agent_end"}));
                    active.store(false, Ordering::SeqCst);
                    emit(json!({"type":"agent_settled"}));
                });
            }
            "steer" => {
                response(&v, json!({}));
                emit(
                    json!({"type":"message_start","message":{"role":"user","content":[{"type":"text","text":v["message"]}]}}),
                );
            }
            "clear_queue" => response(&v, json!({"steering":[],"followUp":[]})),
            "abort" => {
                abort.store(true, Ordering::SeqCst);
                response(&v, json!({}));
            }
            "extension_ui_response" => {
                if let Some(prompt) = dialog.take() {
                    emit(
                        json!({"type":"extension_ui_request","id":"notice","method":"notify","message":v.to_string()}),
                    );
                    response(&prompt, json!({}));
                }
            }
            _ => response(&v, json!({})),
        }
    }
}
