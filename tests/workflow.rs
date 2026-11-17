use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

#[test]
fn watch_keeps_distinct_send_and_chain_outcomes() {
    let wallet = bs58::encode([1; 32]).into_string();
    let output_mint = bs58::encode([3; 32]).into_string();
    let sol = "So11111111111111111111111111111111111111112";
    let signature = bs58::encode([2; 64]).into_string();
    let mut signed = vec![1];
    signed.extend([2; 64]);
    signed.extend([128, 1, 0, 0, 1]); // v0 message, one signer and one account
    signed.extend([1; 32]);
    signed.extend([4; 32]);
    signed.extend([0, 0]); // no instructions or address lookups for the mock RPC
    let directory = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("workflow-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("wire.b64"), STANDARD.encode(signed)).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_solana-swap-journal"))
            .current_dir(&directory)
            .args(args)
            .env_remove("JOURNAL_DB")
            .env_remove("SEND_RPC_URL")
            .env("NO_PROXY", "127.0.0.1")
            .output()
            .unwrap()
    };
    ok(run(&[
        "quote",
        "--input-mint",
        sol,
        "--output-mint",
        &output_mint,
        "--input-raw",
        "100000000",
        "--output-raw",
        "1000",
        "--output",
        "quote.json",
    ]));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let transaction = json!({
        "slot": 42, "blockTime": 1744966356,
        "meta": { "err": null, "fee": 5000, "preBalances":[2000000000, 0],
            "postBalances":[1897995000, 2000000], "preTokenBalances":[],
            "postTokenBalances":[{"accountIndex":1, "owner":wallet, "mint":output_mint,
                "uiTokenAmount":{"amount":"990","decimals":6}}] },
        "transaction":{"signatures":[signature],"message":{"accountKeys":[
            {"pubkey":wallet},{"pubkey":bs58::encode([5;32]).into_string()}]}}
    });
    let server = thread::spawn(move || {
        for (method, status, body) in [
            ("sendTransaction", "429 Too Many Requests", json!(null)),
            (
                "getSignatureStatuses",
                "200 OK",
                json!({"value":[{"slot": 42,"confirmationStatus":"confirmed","err":null}]}),
            ),
            ("getTransaction", "200 OK", transaction),
        ] {
            let mut stream = accept(&listener);
            let request = read_request(&mut stream);
            assert_eq!(request["method"], method);
            if method == "sendTransaction" {
                assert_eq!(request["params"][1]["skipPreflight"], false);
            }
            reply(&mut stream, status, body);
        }
    });
    let args = [
        "--rpc-url",
        &url,
        "watch",
        "--wallet",
        &wallet,
        "--signed-tx",
        "wire.b64",
        "--quote",
        "quote.json",
        "--timeout-secs",
        "5",
        "--submit",
    ];
    assert!(ok(run(&args)).contains("confirmed"));
    server.join().unwrap();
    let duplicate = run(&args);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("signature already in journal"));
    let details = ok(run(&["report", "--trades"]));
    assert!(details.contains(&signature));
    assert!(details.contains(",100000000,9,"));
    assert!(details.contains(",990,6,native_sol_net_balance,"));
    assert!(details.contains(",100.00,"));
    assert!(details.contains("HTTP 429"));
    let summary = ok(run(&["report"]));
    assert!(summary.contains(",1,1,0,0,100.00,5000,0,1,0,1,100.00,1,"));
}

#[test]
fn wallet_import_uses_the_saved_head_and_keeps_failed_attempts() {
    let wallet = bs58::encode([6; 32]).into_string();
    let program = bs58::encode([7; 32]).into_string();
    let signature = bs58::encode([8; 64]).into_string();
    let directory = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("history-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let page = json!([{"signature":signature,"blockTime":1744966356}]);
    let transaction = json!({
        "slot": 42, "blockTime": 1744966356,
        "meta":{"fee":5000,"err":{"InstructionError":[0,{"Custom":42}]}},
        "transaction":{"signatures":[signature],"message":{"accountKeys":[{"pubkey":wallet}],
            "instructions":[{"programId":program}]}}
    });
    let last_signature = signature.clone();
    let server = thread::spawn(move || {
        for (index, (method, body)) in [
            ("getSignaturesForAddress", page.clone()),
            ("getTransaction", transaction),
            ("getSignaturesForAddress", json!([])),
            ("getSignaturesForAddress", page),
        ]
        .into_iter()
        .enumerate()
        {
            let mut stream = accept(&listener);
            let request = read_request(&mut stream);
            assert_eq!(request["method"], method);
            if index == 2 {
                assert_eq!(request["params"][1]["before"], last_signature);
            }
            reply(&mut stream, "200 OK", body);
        }
    });
    let run = |args: &[&str]| {
        ok(Command::new(env!("CARGO_BIN_EXE_solana-swap-journal"))
            .current_dir(&directory)
            .env_remove("JOURNAL_DB")
            .env("NO_PROXY", "127.0.0.1")
            .args(["--rpc-url", &url])
            .args(args)
            .output()
            .unwrap())
    };
    assert!(run(&[
        "import",
        "--wallet",
        &wallet,
        "--program",
        &program,
        "--since",
        "2025-04-18"
    ])
    .contains("Reviewed 1 signatures, saved 1"));
    assert!(run(&["import", "--wallet", &wallet, "--program", &program])
        .contains("Reviewed 0 signatures"));
    server.join().unwrap();
    let details = run(&["report", "--trades"]);
    assert!(details.contains(&signature));
    assert!(details.contains(",42,failed,"));
    assert!(details.contains("InstructionError"));
}

fn reply(stream: &mut TcpStream, status: &str, body: Value) {
    let response = json!({"jsonrpc":"2.0", "id":1, "result":body}).to_string();
    write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
}

fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn accept(listener: &TcpListener) -> TcpStream {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("mock RPC listener: {error}"),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = None;
    loop {
        let mut header = String::new();
        assert_ne!(reader.read_line(&mut header).unwrap(), 0);
        if header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse().unwrap());
            }
        }
    }
    let mut bytes = vec![0; length.unwrap()];
    reader.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
