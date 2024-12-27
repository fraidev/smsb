use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use adaptive_backoff::prelude::Backoff;
use adaptive_backoff::prelude::BackoffBuilder;
use adaptive_backoff::prelude::ExponentialBackoff;
use adaptive_backoff::prelude::ExponentialBackoffBuilder;
use anyhow::Result;
use apalis::prelude::*;
use apalis_cron::CronStream;
use apalis_cron::Schedule;
use atrium_api::app::bsky::feed::post::RecordData;
use bsky_sdk::BskyAgent;
use chrono::Local;
use chrono::{DateTime, Utc};
use dotenv::dotenv;
use separator::Separatable;
use tokio::sync::Mutex;
use tower::load_shed::LoadShedLayer;
use tracing::error;
use tracing::info;
use tweety_rs::TweetyClient;

const DEFAULT_CRONJOB: &str = "0 0,30 13-21 * * Mon-Fri";
const BOVESPA_FETCH_URL: &str = "https://query1.finance.yahoo.com/v8/finance/chart/%5EBVSP?interval=1m&includePrePost=true&events=div%7Csplit%7Cearn&&lang=en-US&region=US";
#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    let cronjob = env::var("CRONJOB").unwrap_or(DEFAULT_CRONJOB.to_string());
    let twitter_client = create_twitter_client();
    let bsky_client = create_bsky_client()
        .await
        .expect("Failed to create Bsky client");
    let schedule = Schedule::from_str(&cronjob).unwrap();
    let bovespa_value = Arc::new(Mutex::new(None::<f64>));
    let backoff = create_backoff();

    info!("Starting SMSB worker with cronjob: {}", cronjob);

    let worker = WorkerBuilder::new("smsb")
        .enable_tracing()
        .layer(LoadShedLayer::new())
        .data(BovespaService {
            bovespa_value,
            twitter_client: Arc::new(twitter_client),
            backoff: Arc::new(Mutex::new(backoff)),
            bsky_client: Arc::new(bsky_client),
        })
        .backend(CronStream::new(schedule))
        .build_fn(execute_bovespa);
    Monitor::new()
        .register(worker)
        .run()
        .await
        .expect("Failed to run monitor");
}

async fn create_bsky_client() -> Result<BskyAgent> {
    let login = env::var("BSKY_LOGIN").expect("BSKY_LOGIN must be set");
    let password = env::var("BSKY_PASSWORD").expect("BSKY_PASSWORD must be set");

    let agent = BskyAgent::builder().build().await?;
    agent.login(login, password).await?;

    Ok(agent)
}

fn create_twitter_client() -> TweetyClient {
    let consumer_key = env::var("TWITTER_CONSUMER_KEY").expect("TWITTER_CONSUMER_KEY must be set");
    let consumer_secret =
        env::var("TWITTER_CONSUMER_SECRET").expect("TWITTER_CONSUMER_SECRET must be set");
    let access_token = env::var("TWITTER_ACCESS_TOKEN").expect("TWITTER_ACCESS_TOKEN must be set");
    let access_secret =
        env::var("TWITTER_ACCESS_SECRET").expect("TWITTER_ACCESS_SECRET must be set");

    TweetyClient::new(
        &consumer_key,
        &access_token,
        &consumer_secret,
        &access_secret,
    )
}

#[derive(Clone)]
struct BovespaService {
    bovespa_value: Arc<Mutex<Option<f64>>>,
    twitter_client: Arc<TweetyClient>,
    backoff: Arc<Mutex<ExponentialBackoff>>,
    bsky_client: Arc<BskyAgent>,
}
impl BovespaService {
    async fn execute(&self, job: Job) -> Result<()> {
        let mut backoff = self.backoff.lock().await;
        loop {
            match self.execute_inner(job.clone()).await {
                Ok(_) => {
                    backoff.reset();
                    break;
                }
                Err(e) => {
                    error!("Failed to execute job: {}", e);
                    tokio::time::sleep(backoff.wait()).await;
                }
            }
        }
        Ok(())
    }

    async fn execute_inner(&self, job: Job) -> Result<()> {
        dbg!(&job.0);
        let new_value = fetch_bovespa().await?;
        let mut guard = self.bovespa_value.lock().await;
        let last_value = guard.unwrap_or(new_value);
        *guard = Some(new_value);
        drop(guard);

        let formatted_value = separate_decimals_brazilian(new_value);

        let msg = if (new_value - last_value).abs() < f64::EPSILON {
            info!(
                "A Bovespa não mudou :| - {} às {}",
                formatted_value,
                Local::now().format("%I:%M %p")
            );
            return Ok(());
        } else if new_value > last_value {
            format!(
                "A Bovespa subiu :) - {} às {}",
                formatted_value,
                Local::now().format("%I:%M %p")
            )
        } else {
            format!(
                "A Bovespa caiu :( - {} às {}",
                formatted_value,
                Local::now().format("%I:%M %p")
            )
        };
        info!("{}", msg);
        post_tweet(self.twitter_client.clone(), &msg).await;
        post_bsky(self.bsky_client.clone(), &msg).await?;
        Ok(())
    }
}

async fn fetch_bovespa() -> Result<f64> {
    info!("Fetching Bovespa value from {}", BOVESPA_FETCH_URL);
    let client = reqwest::ClientBuilder::new()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64)")
        .build()?;

    let response = client
        .get(BOVESPA_FETCH_URL)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let value = response["chart"]["result"][0]["meta"]["regularMarketPrice"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("Failed to parse value"))?;
    Ok(value)
}

async fn post_tweet(client: Arc<TweetyClient>, message: &str) {
    match client.post_tweet(message, None).await {
        Ok(_) => info!("Tweet posted successfully: {}", message),
        Err(e) => error!("Failed to post tweet: {}", e),
    }
}

async fn post_bsky(client: Arc<BskyAgent>, message: &str) -> Result<()> {
    client
        .create_record(RecordData {
            created_at: atrium_api::types::string::Datetime::now(),
            embed: None,
            entities: None,
            facets: None,
            labels: None,
            langs: None,
            reply: None,
            tags: None,
            text: message.to_string(),
        })
        .await?;
    Ok(())
}

#[derive(Default, Debug, Clone)]
struct Job(DateTime<Utc>);
impl From<DateTime<Utc>> for Job {
    fn from(t: DateTime<Utc>) -> Self {
        Job(t)
    }
}
async fn execute_bovespa(job: Job, svc: Data<BovespaService>) {
    match svc.execute(job).await {
        Ok(_) => info!("Job executed successfully"),
        Err(e) => error!("Failed to execute bovespa service: {}", e),
    }
}

fn create_backoff() -> ExponentialBackoff {
    ExponentialBackoffBuilder::default()
        .factor(1.1)
        .min(Duration::from_secs(1))
        .max(Duration::from_secs(300))
        .build()
        .unwrap()
}

fn separate_decimals(value: f64) -> String {
    let separated = value.separated_string();
    let decimal_separated = separated.split('.').collect::<Vec<&str>>();
    let integer = decimal_separated[0];
    let decimal = decimal_separated[1];
    if decimal.len() > 2 {
        return integer.to_owned() + "." + &decimal[0..2];
    }

    if decimal.len() == 1 {
        return integer.to_owned() + "." + decimal + "0";
    }
    separated
}

fn separate_decimals_brazilian(value: f64) -> String {
    let normal = separate_decimals(value);

    let decimal_separated = normal.split('.').collect::<Vec<&str>>();
    let integer = decimal_separated[0];
    let decimal = decimal_separated[1];
    let mut formatted = integer.replace(",", ".");
    formatted.push(',');
    formatted.push_str(decimal);
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_decimal_places() {
        assert_eq!(separate_decimals(2312321321.32323), "2,312,321,321.32");
        assert_eq!(separate_decimals(2312321321.3), "2,312,321,321.30");
        assert_eq!(separate_decimals(120276.98), "120,276.98");
        assert_eq!(separate_decimals(0.98), "0.98");
    }

    #[test]
    fn test_separate_decimal_brazilian_style() {
        assert_eq!(
            separate_decimals_brazilian(2312321321.32323),
            "2.312.321.321,32"
        );
        assert_eq!(
            separate_decimals_brazilian(2312321321.3),
            "2.312.321.321,30"
        );
        assert_eq!(separate_decimals_brazilian(120276.98), "120.276,98");
        assert_eq!(separate_decimals_brazilian(0.98), "0,98");
    }
}
