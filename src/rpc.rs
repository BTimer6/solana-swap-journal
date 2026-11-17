use std::{fmt, thread, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use reqwest::{blocking::Client, StatusCode};
use serde_json::{json, Value};

pub struct Rpc {
    client: Client,
    url: reqwest::Url,
}

#[derive(Debug)]
struct HttpFailure(StatusCode);

impl fmt::Display for HttpFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpFailure {}

impl Rpc {
    pub fn new(url: &str) -> Result<Self> {
        let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid RPC URL"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "RPC URL must use HTTP(S)"
        );
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()?,
            url,
        })
    }

    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let response = self
            .client
            .post(self.url.clone())
            .timeout(timeout)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .map_err(|err| err.without_url())
            .with_context(|| method.to_owned())?;
        let status = response.status();
        if !status.is_success() {
            return Err(HttpFailure(status)).with_context(|| method.to_owned());
        }
        let body: Value = response
            .json()
            .map_err(|err| err.without_url())
            .with_context(|| format!("{method}: invalid JSON response"))?;
        if let Some(error) = body.get("error").filter(|error| !error.is_null()) {
            bail!(
                "{method}: RPC error {}",
                error.get("code").unwrap_or(&Value::Null)
            );
        }
        body.get("result")
            .cloned()
            .with_context(|| format!("{method}: missing result"))
    }

    pub fn read(&self, method: &str, params: Value) -> Result<Value> {
        let mut delays = [1, 3, 6].into_iter();
        loop {
            let result = self.call(method, params.clone(), Duration::from_secs(15));
            let retryable = result
                .as_ref()
                .err()
                .and_then(|err| err.downcast_ref::<HttpFailure>())
                .is_some_and(|err| matches!(err.0.as_u16(), 429 | 502..=504));
            match delays.next() {
                Some(delay) if retryable => thread::sleep(Duration::from_secs(delay)),
                _ => return result,
            }
        }
    }

    pub fn transaction(&self, signature: &str, commitment: &str) -> Result<Option<Value>> {
        let result = self.read("getTransaction", json!([signature, {
            "encoding": "jsonParsed", "maxSupportedTransactionVersion": 0, "commitment": commitment
        }]))?;
        Ok((!result.is_null()).then_some(result))
    }
}
