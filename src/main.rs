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
use chrono::Local;
use chrono::{DateTime, Utc};
use dotenv::dotenv;
use tokio::sync::Mutex;
use tower::load_shed::LoadShedLayer;
use tracing::error;
use tracing::info;
use tweety_rs::TweetyClient;

const DEFAULT_CRONJOB: &str = "0 0,30 13-21 * * Mon-Fri";

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt::init();

    let cronjob = env::var("CRONJOB").unwrap_or(DEFAULT_CRONJOB.to_string());
    let twitter_client = create_twitter_client();
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
        })
        .backend(CronStream::new(schedule))
        .build_fn(execute_bovespa);
    Monitor::new()
        .register(worker)
        .run()
        .await
        .expect("Failed to run monitor");
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

        let msg = if (new_value - last_value).abs() < f64::EPSILON {
            info!(
                "A Bovespa não mudou :| - {:.2} às {}",
                new_value,
                Local::now().format("%I:%M %p")
            );
            return Ok(());
        } else if new_value > last_value {
            format!(
                "A Bovespa subiu :) - {:.2} às {}",
                new_value,
                Local::now().format("%I:%M %p")
            )
        } else {
            format!(
                "A Bovespa caiu :( - {:.2} às {}",
                new_value,
                Local::now().format("%I:%M %p")
            )
        };
        info!("{}", msg);
        post_tweet(self.twitter_client.clone(), &msg).await;
        Ok(())
    }
}

async fn fetch_bovespa() -> Result<f64> {
    let url = "https://query1.finance.yahoo.com/v8/finance/chart/%5EBVSP?interval=1m&includePrePost=true&events=div%7Csplit%7Cearn&&lang=en-US&region=US";
    info!("Fetching Bovespa value from {}", url);
    let client = reqwest::ClientBuilder::new()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64)")
        .build()?;

    let response = client
        .get(url)
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
