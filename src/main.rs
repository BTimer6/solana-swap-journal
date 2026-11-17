mod report;
mod rpc;
mod store;
mod swap;

use std::{
    fs::{self, File},
    io,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use rpc::Rpc;
use store::{Quote, Store};

/// Local journal for Solana swap fills and confirmation times.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(
        long,
        env = "JOURNAL_DB",
        default_value = "journal.sqlite3",
        global = true
    )]
    db: PathBuf,
    #[arg(
        long,
        env = "RPC_URL",
        default_value = "https://api.mainnet-beta.solana.com",
        global = true,
        hide_env_values = true
    )]
    rpc_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import a known swap signature or scan a wallet for selected swap programs.
    Import(Import),
    /// Record the currently displayed ExactIn quote as JSON on stdout.
    Quote {
        #[arg(long, value_parser = public_key)]
        input_mint: String,
        #[arg(long, value_parser = public_key)]
        output_mint: String,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        input_raw: u64,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        output_raw: u64,
        /// Create a new quote file. Defaults to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Submit one signed swap transaction and track its confirmation.
    Watch(Watch),
    /// Export daily summaries or detailed trades as CSV.
    Report {
        #[arg(long, value_parser = public_key)]
        wallet: Option<String>,
        /// First included date, UTC (YYYY-MM-DD).
        #[arg(long)]
        since: Option<NaiveDate>,
        /// Last included date, UTC (YYYY-MM-DD).
        #[arg(long)]
        until: Option<NaiveDate>,
        #[arg(long)]
        trades: bool,
        /// Create a new CSV file. Defaults to stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Args)]
struct Import {
    #[arg(long, value_parser = public_key)]
    wallet: String,
    #[arg(long, value_parser = transaction_signature, conflicts_with_all = ["program", "since", "until"])]
    signature: Option<String>,
    /// A swap program address. Repeat for multiple programs.
    #[arg(long, value_parser = public_key, required_unless_present = "signature")]
    program: Vec<String>,
    /// First included date, UTC. Required for the first wallet import.
    #[arg(long)]
    since: Option<NaiveDate>,
    /// Last included date, UTC. Bounded imports do not advance the wallet cursor.
    #[arg(long, requires = "since")]
    until: Option<NaiveDate>,
}

#[derive(Args)]
struct Watch {
    #[arg(long, value_parser = public_key)]
    wallet: String,
    /// File containing one fully signed transaction encoded as base64.
    #[arg(long)]
    signed_tx: PathBuf,
    /// Optional ExactIn quote JSON from before signing.
    #[arg(long)]
    quote: Option<PathBuf>,
    /// Explicitly submit this transaction to the configured network.
    #[arg(long, required = true)]
    submit: bool,
    /// Optional separate sendTransaction endpoint. Status checks use RPC_URL.
    #[arg(long, env = "SEND_RPC_URL", hide_env_values = true)]
    send_rpc_url: Option<String>,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(2..=600))]
    timeout_secs: u64,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..=10000))]
    poll_ms: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Import(args) => import(&Store::open(&cli.db)?, &Rpc::new(&cli.rpc_url)?, args),
        Command::Watch(args) => watch(&Store::open(&cli.db)?, &Rpc::new(&cli.rpc_url)?, args),
        Command::Quote {
            input_mint,
            output_mint,
            input_raw,
            output_raw,
            output,
        } => {
            ensure!(input_mint != output_mint, "quote mints must differ");
            let quote = Quote {
                captured_at: Utc::now(),
                input_mint,
                output_mint,
                input_raw: input_raw.to_string(),
                output_raw: output_raw.to_string(),
            };
            let mut writer: Box<dyn io::Write> = match output {
                Some(path) => Box::new(File::create_new(path)?),
                None => Box::new(io::stdout().lock()),
            };
            serde_json::to_writer_pretty(&mut writer, &quote)?;
            writeln!(writer)?;
            Ok(())
        }
        Command::Report {
            wallet,
            since,
            until,
            trades,
            output,
        } => {
            check_dates(since, until)?;
            let entries = Store::open(&cli.db)?.entries(wallet.as_deref())?;
            if let Some(path) = output {
                let mut out = io::BufWriter::new(
                    File::create_new(&path)
                        .with_context(|| format!("creating {}", path.display()))?,
                );
                report::write_report(&mut out, entries, since, until, trades)?;
                io::Write::flush(&mut out)?;
                Ok(())
            } else {
                report::write_report(io::stdout().lock(), entries, since, until, trades)
            }
        }
    }
}

fn import(store: &Store, rpc: &Rpc, mut args: Import) -> Result<()> {
    if let Some(signature) = args.signature {
        let (tx, finalized) = match rpc.transaction(&signature, "finalized")? {
            Some(tx) => (tx, true),
            None => (
                rpc.transaction(&signature, "confirmed")?
                    .context("signature is not available at confirmed commitment")?,
                false,
            ),
        };
        let chain = swap::parse(&tx, &signature, &args.wallet)?;
        store.save_chain(&args.wallet, &signature, &chain, finalized)?;
        println!("Saved {signature}");
        return Ok(());
    }
    check_dates(args.since, args.until)?;
    args.program.sort_unstable();
    args.program.dedup();
    let key = args.program.join(",");
    let previous = store.head(&args.wallet, &key)?;
    ensure!(
        previous.is_some() || args.since.is_some(),
        "first wallet import requires --since YYYY-MM-DD (UTC)"
    );
    let stop_signature = previous.as_deref().filter(|_| args.since.is_none());
    let mut head = None;
    let mut before = None::<String>;
    let mut reviewed = 0;
    let mut saved = 0;
    'pages: loop {
        let mut config = json!({"commitment": "finalized", "limit": 100});
        if let Some(cursor) = &before {
            config["before"] = json!(cursor);
        }
        let result = rpc.read("getSignaturesForAddress", json!([args.wallet, config]))?;
        let page = result
            .as_array()
            .context("expected signature list from RPC")?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let signature = row["signature"]
                .as_str()
                .context("missing listed signature")?;
            head.get_or_insert_with(|| signature.to_owned());
            if Some(signature) == stop_signature {
                break 'pages;
            }
            let day = row["blockTime"]
                .as_i64()
                .and_then(|time| DateTime::from_timestamp(time, 0))
                .map(|time| time.date_naive());
            if day.zip(args.since).is_some_and(|(day, since)| day < since) {
                break 'pages;
            }
            if day.zip(args.until).is_some_and(|(day, until)| day > until) {
                continue;
            }
            reviewed += 1;
            if store.finalized(&args.wallet, signature)? {
                continue;
            }
            let tx = rpc.transaction(signature, "finalized")?.with_context(|| {
                format!("history unavailable for {signature}; retry with an archival RPC")
            })?;
            if swap::uses_program(&tx, &args.program) {
                let chain = swap::parse(&tx, signature, &args.wallet)?;
                let day = chain
                    .time
                    .and_then(|time| DateTime::from_timestamp(time, 0))
                    .map(|time| time.date_naive());
                if args
                    .since
                    .is_some_and(|since| day.is_none_or(|day| day < since))
                    || args
                        .until
                        .is_some_and(|until| day.is_none_or(|day| day > until))
                {
                    continue;
                }
                store.save_chain(&args.wallet, signature, &chain, true)?;
                saved += 1;
            }
        }
        let next = page.last().context("empty signature page")?["signature"]
            .as_str()
            .context("last history item missing a signature")?;
        ensure!(
            before.as_deref() != Some(next),
            "RPC repeated a history page"
        );
        before = Some(next.to_owned());
    }
    if let Some(head) = head.filter(|_| args.until.is_none()) {
        store.advance_head(&args.wallet, &key, previous.as_deref(), &head)?;
    }
    println!("Reviewed {reviewed} signatures, saved {saved} swaps or attempts");
    Ok(())
}

fn watch(store: &Store, rpc: &Rpc, args: Watch) -> Result<()> {
    let encoded = fs::read_to_string(&args.signed_tx).context("reading signed transaction file")?;
    let encoded = encoded.trim();
    let signature = signed_signature(encoded, &args.wallet)?;
    let quote = args
        .quote
        .as_ref()
        .map(fs::read)
        .transpose()?
        .as_deref()
        .map(read_quote)
        .transpose()?;
    let sender = args.send_rpc_url.as_deref().map(Rpc::new).transpose()?;
    let sender = sender.as_ref().unwrap_or(rpc);
    store.begin_send(
        &args.wallet,
        &signature,
        Utc::now().timestamp_millis(),
        quote.as_ref(),
    )?;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.timeout_secs);
    let send = sender
        .call(
            "sendTransaction",
            json!([encoded, {
                "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed"
            }]),
            Duration::from_secs(12).min(Duration::from_secs(args.timeout_secs)),
        )
        .and_then(|reply| {
            ensure!(
                reply.as_str() == Some(signature.as_str()),
                "sendTransaction returned a different signature"
            );
            Ok(())
        });
    let send_error = send.err().map(|error| format!("{error:#}"));
    store.send_response(
        &args.wallet,
        &signature,
        elapsed_ms(start),
        send_error.as_deref(),
    )?;
    eprintln!("Tracking {signature}");
    if let Some(error) = send_error {
        eprintln!("{error}; checking the signature before deciding its outcome");
    }
    let mut last_poll_error = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = rpc.call(
            "getSignatureStatuses",
            json!([[signature], {"searchTransactionHistory": true}]),
            Duration::from_secs(8).min(remaining),
        );
        match result {
            Ok(status) => {
                let status = &status["value"][0];
                let commitment = status["confirmationStatus"].as_str();
                if matches!(commitment, Some("confirmed" | "finalized")) {
                    let elapsed = elapsed_ms(start);
                    let state = if status["err"].is_null() {
                        "confirmed"
                    } else {
                        "chain_failed"
                    };
                    store.finish_tracking(&args.wallet, &signature, state, Some(elapsed))?;
                    match rpc.transaction(&signature, "confirmed") {
                        Ok(Some(tx)) => {
                            let chain = swap::parse(&tx, &signature, &args.wallet)?;
                            store.save_chain(&args.wallet, &signature, &chain, false)?;
                        }
                        Ok(None) => eprintln!("Transaction metadata not available yet; refresh using import --signature"),
                        Err(error) => eprintln!("Could not read metadata: {error:#}; refresh using import --signature"),
                    }
                    println!("{signature}: {state} in {elapsed} ms");
                    ensure!(
                        state == "confirmed",
                        "transaction execution failed on chain"
                    );
                    return Ok(());
                }
                last_poll_error = None;
            }
            Err(error) => last_poll_error = Some(error),
        }
        thread::sleep(
            Duration::from_millis(args.poll_ms)
                .min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    store.finish_tracking(&args.wallet, &signature, "timeout", None)?;
    if let Some(error) = last_poll_error {
        eprintln!("Last status check: {error:#}");
    }
    anyhow::bail!("confirmation timeout for {signature}; refresh using import --signature");
}

fn elapsed_ms(start: Instant) -> i64 {
    start.elapsed().as_millis() as i64
}

fn signed_signature(encoded: &str, wallet: &str) -> Result<String> {
    ensure!(
        encoded.len() <= 1644,
        "base64 transaction exceeds the 1232-byte wire limit"
    );
    let bytes = STANDARD
        .decode(encoded)
        .context("invalid base64 transaction")?;
    // At 64 bytes per signature, a packet this size can only have a one-byte shortvec prefix.
    let count = usize::from(*bytes.first().context("empty transaction")?);
    ensure!(
        bytes.len() <= 1232 && count > 0 && bytes.len() > 1 + count * 64,
        "invalid signature vector"
    );
    let signatures = &bytes[1..1 + count * 64];
    ensure!(
        signatures
            .as_chunks::<64>()
            .0
            .iter()
            .all(|sig| sig.iter().any(|byte| *byte != 0)),
        "transaction contains an empty signature"
    );
    let message = &bytes[1 + count * 64..];
    let header = if message[0] == 128 {
        &message[1..]
    } else {
        message
    };
    ensure!(
        header.len() > 3 && usize::from(header[0]) == count,
        "unsupported or invalid transaction header"
    );
    // The wire limit permits fewer than 128 static accounts and therefore a one-byte count.
    let accounts_len = usize::from(header[3]);
    ensure!(
        accounts_len >= count && header.len() > 4 + accounts_len * 32 + 32,
        "invalid transaction account list"
    );
    let wallet = bs58::decode(wallet)
        .into_vec()
        .context("invalid signer address")?;
    ensure!(
        header[4..4 + count * 32]
            .as_chunks::<32>()
            .0
            .iter()
            .any(|key| key.as_slice() == wallet),
        "wallet must be a signer of the transaction"
    );
    Ok(bs58::encode(&signatures[..64]).into_string())
}

fn read_quote(bytes: &[u8]) -> Result<Quote> {
    let quote: Quote = serde_json::from_slice(bytes).context("invalid quote JSON")?;
    ensure!(
        public_key(&quote.input_mint).is_ok() && public_key(&quote.output_mint).is_ok(),
        "invalid quote mint"
    );
    ensure!(
        quote.input_mint != quote.output_mint,
        "quote mints must differ"
    );
    ensure!(
        quote.input_raw.parse::<u64>().is_ok_and(|n| n > 0)
            && quote.output_raw.parse::<u64>().is_ok_and(|n| n > 0),
        "quote amounts must be positive integer strings"
    );
    ensure!(
        quote.captured_at <= Utc::now(),
        "quote capture time must be in the past"
    );
    Ok(quote)
}

fn check_dates(since: Option<NaiveDate>, until: Option<NaiveDate>) -> Result<()> {
    ensure!(
        !since.zip(until).is_some_and(|(since, until)| since > until),
        "--since must be on or before --until"
    );
    Ok(())
}

fn public_key(value: &str) -> Result<String, String> {
    base58(value, 32)
}

fn transaction_signature(value: &str) -> Result<String, String> {
    base58(value, 64)
}

fn base58(value: &str, length: usize) -> Result<String, String> {
    let mut buffer = [0u8; 64];
    if bs58::decode(value).onto(&mut buffer[..length]) == Ok(length) {
        Ok(value.to_owned())
    } else {
        Err(format!("expected a {length}-byte base58 value"))
    }
}
