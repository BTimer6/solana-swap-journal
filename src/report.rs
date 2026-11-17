use std::{collections::BTreeMap, io::Write};

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};

use crate::store::Entry;

impl Entry {
    fn chain_status(&self) -> &str {
        match (&self.chain, self.send_state.as_deref()) {
            (Some(chain), _) if chain.error.is_some() => "failed",
            (Some(_), _) | (None, Some("confirmed")) => "success",
            (None, Some("chain_failed")) => "failed",
            _ => "unresolved",
        }
    }

    fn date_time(&self) -> Option<DateTime<Utc>> {
        self.chain
            .as_ref()
            .and_then(|chain| chain.time)
            .and_then(|time| DateTime::from_timestamp(time, 0))
            .or_else(|| {
                self.submitted_at_ms
                    .and_then(DateTime::from_timestamp_millis)
            })
    }

    fn pair(&self) -> (&str, &str) {
        if let Some((input, output)) = self
            .chain
            .as_ref()
            .and_then(|chain| chain.input.as_ref().zip(chain.output.as_ref()))
        {
            return (&input.mint, &output.mint);
        }
        self.quote
            .as_ref()
            .map_or(("", ""), |quote| (&quote.input_mint, &quote.output_mint))
    }

    fn slippage_bps(&self) -> Option<f64> {
        let quote = self.quote.as_ref()?;
        let chain = self.chain.as_ref()?;
        let (input, output) = (chain.input.as_ref()?, chain.output.as_ref()?);
        let quote_in: u128 = quote.input_raw.parse().ok()?;
        let quote_out: u128 = quote.output_raw.parse().ok()?;
        if quote_out == 0
            || input.mint != quote.input_mint
            || output.mint != quote.output_mint
            || input.raw.parse::<u128>().ok()? != quote_in
        {
            return None;
        }
        let actual: i128 = output.raw.parse().ok()?;
        Some((i128::try_from(quote_out).ok()? - actual) as f64 * 10_000.0 / quote_out as f64)
    }
}

#[derive(Default)]
struct Stats {
    total: usize,
    succeeded: usize,
    failed: usize,
    fees: u128,
    missing_amounts: usize,
    send_errors: usize,
    timeouts: usize,
    slippage: Vec<f64>,
    confirmations: Vec<i64>,
}

pub fn write_report(
    mut out: impl Write,
    mut entries: Vec<Entry>,
    since: Option<NaiveDate>,
    until: Option<NaiveDate>,
    details: bool,
) -> Result<()> {
    entries.retain(|entry| {
        let date = entry.date_time().map(|dt| dt.date_naive());
        since.is_none_or(|since| date.is_some_and(|day| day >= since))
            && until.is_none_or(|until| date.is_some_and(|day| day <= until))
    });
    if details {
        entries.sort_by(|a, b| {
            (a.date_time(), &a.wallet, &a.signature).cmp(&(b.date_time(), &b.wallet, &b.signature))
        });
        return write_details(&mut out, entries);
    }
    writeln!(out, "date_utc,input_mint,output_mint,total,chain_success,chain_failure,unresolved,chain_success_pct,wallet_network_fee_lamports,unresolved_amounts,send_errors,timeouts,slippage_samples,slippage_p50_bps,confirmation_samples,confirmation_p50_ms,confirmation_p95_ms")?;
    let mut grouped = BTreeMap::<_, Stats>::new();
    for entry in &entries {
        let pair = entry.pair();
        let group = grouped
            .entry((entry.date_time().map(|dt| dt.date_naive()), pair.0, pair.1))
            .or_default();
        group.total += 1;
        match entry.chain_status() {
            "success" => {
                group.succeeded += 1;
                group.missing_amounts += usize::from(
                    entry
                        .chain
                        .as_ref()
                        .is_none_or(|chain| chain.input.is_none()),
                );
                group.confirmations.extend(entry.confirm_ms);
            }
            "failed" => group.failed += 1,
            _ => (),
        }
        if let Some(chain) = &entry.chain {
            if entry.wallet == chain.fee_payer {
                group.fees += u128::from(chain.fee);
            }
        }
        group.send_errors += usize::from(entry.send_error.is_some());
        group.timeouts += usize::from(entry.send_state.as_deref() == Some("timeout"));
        if let Some(value) = entry.slippage_bps() {
            group.slippage.push(value);
        }
    }
    for ((date, input, output), mut group) in grouped {
        group.slippage.sort_by(f64::total_cmp);
        group.confirmations.sort_unstable();
        let resolved = group.succeeded + group.failed;
        let percent = (resolved > 0)
            .then(|| format!("{:.2}", group.succeeded as f64 * 100.0 / resolved as f64));
        line(
            &mut out,
            &[
                field(date),
                input.into(),
                output.into(),
                group.total.to_string(),
                group.succeeded.to_string(),
                group.failed.to_string(),
                (group.total - resolved).to_string(),
                field(percent),
                group.fees.to_string(),
                group.missing_amounts.to_string(),
                group.send_errors.to_string(),
                group.timeouts.to_string(),
                group.slippage.len().to_string(),
                field(percentile(&group.slippage, 50).map(|v| format!("{v:.2}"))),
                group.confirmations.len().to_string(),
                field(percentile(&group.confirmations, 50)),
                field(percentile(&group.confirmations, 95)),
            ],
        )?;
    }
    Ok(())
}

fn write_details(out: &mut impl Write, entries: Vec<Entry>) -> Result<()> {
    writeln!(out, "wallet,signature,block_time_utc,slot,chain_status,chain_error,fee_payer,network_fee_lamports,input_mint,input_raw,input_decimals,output_mint,output_raw,output_decimals,amount_note,quote_at_utc,quote_input_raw,quote_output_raw,slippage_bps,submitted_at_utc,send_state,response_ms,confirm_ms,send_error,quote_input_mint,quote_output_mint")?;
    for entry in entries {
        let chain = entry.chain.as_ref();
        let quote = entry.quote.as_ref();
        let input = chain.and_then(|chain| chain.input.as_ref());
        let output = chain.and_then(|chain| chain.output.as_ref());
        let state = entry.chain_status();
        line(
            out,
            &[
                entry.wallet.clone(),
                entry.signature.clone(),
                timestamp(
                    chain
                        .and_then(|chain| chain.time)
                        .and_then(|time| DateTime::from_timestamp(time, 0)),
                ),
                field(chain.map(|chain| chain.slot)),
                state.into(),
                field(chain.and_then(|chain| chain.error.as_ref())),
                field(chain.map(|chain| &chain.fee_payer)),
                field(chain.map(|chain| chain.fee)),
                field(input.map(|leg| &leg.mint)),
                field(input.map(|leg| &leg.raw)),
                field(input.map(|leg| leg.decimals)),
                field(output.map(|leg| &leg.mint)),
                field(output.map(|leg| &leg.raw)),
                field(output.map(|leg| leg.decimals)),
                field(chain.and_then(|chain| chain.amount_note.as_ref())),
                timestamp(quote.map(|quote| quote.captured_at)),
                field(quote.map(|quote| &quote.input_raw)),
                field(quote.map(|quote| &quote.output_raw)),
                field(entry.slippage_bps().map(|v| format!("{v:.2}"))),
                timestamp(
                    entry
                        .submitted_at_ms
                        .and_then(DateTime::from_timestamp_millis),
                ),
                field(entry.send_state),
                field(entry.response_ms),
                field(entry.confirm_ms),
                field(entry.send_error),
                field(quote.map(|quote| &quote.input_mint)),
                field(quote.map(|quote| &quote.output_mint)),
            ],
        )?;
    }
    Ok(())
}

fn percentile<T>(values: &[T], p: usize) -> Option<&T> {
    (!values.is_empty()).then(|| &values[(values.len() * p).div_ceil(100) - 1])
}

fn timestamp(value: Option<DateTime<Utc>>) -> String {
    field(value.map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
}

fn field(value: Option<impl ToString>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn line(out: &mut impl Write, fields: &[String]) -> Result<()> {
    for (index, field) in fields.iter().enumerate() {
        if index != 0 {
            write!(out, ",")?;
        }
        if field.contains([',', '"', '\n', '\r']) {
            write!(out, "\"{}\"", field.replace('"', "\"\""))?;
        } else {
            write!(out, "{field}")?;
        }
    }
    writeln!(out)?;
    Ok(())
}
