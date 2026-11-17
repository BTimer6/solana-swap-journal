use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SOL: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Deserialize, Serialize)]
pub struct Leg {
    pub mint: String,
    pub raw: String,
    pub decimals: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Chain {
    pub slot: u64,
    pub time: Option<i64>,
    pub fee: u64,
    pub fee_payer: String,
    pub error: Option<Value>,
    pub input: Option<Leg>,
    pub output: Option<Leg>,
    pub amount_note: Option<String>,
}

pub fn uses_program(tx: &Value, programs: &[String]) -> bool {
    let matches = |ix: &Value| {
        ix["programId"]
            .as_str()
            .is_some_and(|id| programs.iter().any(|program| program == id))
    };
    let outer = tx
        .pointer("/transaction/message/instructions")
        .and_then(Value::as_array);
    let inner = tx
        .pointer("/meta/innerInstructions")
        .and_then(Value::as_array);
    outer.into_iter().flatten().any(matches)
        || inner
            .into_iter()
            .flatten()
            .filter_map(|group| group["instructions"].as_array())
            .flatten()
            .any(matches)
}

pub fn parse(tx: &Value, signature: &str, owner: &str) -> Result<Chain> {
    ensure!(
        tx.pointer("/transaction/signatures/0")
            .and_then(Value::as_str)
            == Some(signature),
        "RPC returned a different transaction"
    );
    let accounts = tx
        .pointer("/transaction/message/accountKeys")
        .and_then(Value::as_array)
        .context("missing parsed transaction account list")?;
    let owner_index = accounts
        .iter()
        .position(|account| account["pubkey"].as_str() == Some(owner))
        .context("wallet is not a transaction account")?;
    let fee_payer = accounts
        .first()
        .and_then(|account| account["pubkey"].as_str())
        .context("missing fee payer")?
        .to_string();
    let meta = &tx["meta"];
    let error = meta
        .get("err")
        .context("missing transaction execution status")?;
    let mut chain = Chain {
        slot: tx["slot"].as_u64().context("missing transaction slot")?,
        time: tx["blockTime"].as_i64(),
        fee: meta["fee"].as_u64().context("missing network fee")?,
        fee_payer,
        error: (!error.is_null()).then(|| error.clone()),
        input: None,
        output: None,
        amount_note: None,
    };
    if chain.error.is_some() {
        return Ok(chain);
    }
    let Some(pre) = meta["preTokenBalances"].as_array() else {
        chain.amount_note = Some("missing_token_metadata".into());
        return Ok(chain);
    };
    let Some(post) = meta["postTokenBalances"].as_array() else {
        chain.amount_note = Some("missing_token_metadata".into());
        return Ok(chain);
    };
    let mut deltas = BTreeMap::new();
    let mut native = if owner_index == 0 {
        i128::from(chain.fee)
    } else {
        0
    };
    let mut saw_wrapped_sol = false;
    for (rows, balances, sign) in [
        (pre, &meta["preBalances"], -1),
        (post, &meta["postBalances"], 1),
    ] {
        let mut owned_accounts = BTreeSet::from([owner_index]);
        for token in rows
            .iter()
            .filter(|token| token["owner"].as_str() == Some(owner))
        {
            let index = token["accountIndex"]
                .as_u64()
                .context("missing token account index")?;
            owned_accounts.insert(usize::try_from(index)?);
            let mint = token["mint"].as_str().context("missing token mint")?;
            if mint == SOL {
                saw_wrapped_sol = true;
                continue;
            }
            let amount = token["uiTokenAmount"]["amount"]
                .as_str()
                .context("missing token amount")?
                .parse::<u64>()
                .context("invalid token amount")?;
            let decimals = token["uiTokenAmount"]["decimals"]
                .as_u64()
                .context("missing token decimals")?;
            let entry = deltas.entry(mint).or_insert((0_i128, decimals));
            ensure!(entry.1 == decimals, "token decimals disagree for {mint}");
            entry.0 += sign * i128::from(amount);
        }
        for index in owned_accounts {
            let lamports = balances[index]
                .as_u64()
                .context("missing lamport balance")?;
            native += sign * i128::from(lamports);
        }
    }
    deltas.retain(|_, (amount, _)| *amount != 0);
    if native != 0 && (deltas.len() == 1 || saw_wrapped_sol) {
        deltas.insert(SOL, (native, 9));
    }
    if deltas.len() == 2 {
        for (mint, (amount, decimals)) in deltas {
            let leg = Leg {
                mint: mint.to_owned(),
                raw: amount.unsigned_abs().to_string(),
                decimals,
            };
            if amount < 0 {
                chain.input = Some(leg);
            } else {
                chain.output = Some(leg);
            }
        }
    }
    if chain.input.is_none() || chain.output.is_none() {
        chain.input = None;
        chain.output = None;
        chain.amount_note = Some("ambiguous_balance_changes".into());
    } else if chain.input.as_ref().is_some_and(|leg| leg.mint == SOL)
        || chain.output.as_ref().is_some_and(|leg| leg.mint == SOL)
    {
        chain.amount_note = Some("native_sol_net_balance".into());
    }
    Ok(chain)
}
