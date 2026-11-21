//! Cliente asíncrono de la API.

use std::time::Duration;

use omni_core::ServerStatus;
use omni_queue::{JobInfo, NewJob, Priority, QueueSettings};
use reqwest::{Method, RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::paths::{Endpoint, Paths};
use crate::types::{AddResponse, ErrorResponse, Ids, PriorityRequest, SpeedLimitRequest, Status};

/// Errores del cliente.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("no se pudo hablar con el servicio de OmniLoad: {0}")]
    Http(#[from] reqwest::Error),
    /// El servicio contestó con un error (por ejemplo, un enlace no válido).
    #[error("{0}")]
    Api(String),
}

/// Cliente de un servicio en marcha.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(endpoint: &Endpoint) -> Self {
        let http = reqwest::Client::builder()
            // Nunca a través de un proxy: el servicio está en este equipo.
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()
            .expect("cliente HTTP");
        Self {
            http,
            base: format!("http://127.0.0.1:{}/api/v1", endpoint.port),
            token: endpoint.token.clone(),
        }
    }

    /// Se conecta al servicio en marcha, si lo hay y contesta.
    pub async fn connect(paths: &Paths) -> Option<Self> {
        let ep = Endpoint::read(&paths.endpoint)?;
        let client = Self::new(&ep);
        client.status().await.ok().map(|_| client)
    }

    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn check(resp: Response) -> Result<Response, ClientError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        match resp.json::<ErrorResponse>().await {
            Ok(e) => Err(ClientError::Api(e.error)),
            Err(_) => Err(ClientError::Api(format!("el servicio respondió {status}"))),
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let resp = self.req(Method::GET, path).send().await?;
        Ok(Self::check(resp).await?.json().await?)
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Response, ClientError> {
        let mut req = self.req(method, path);
        if let Some(b) = body {
            req = req.json(b);
        }
        Self::check(req.send().await?).await
    }

    async fn post<B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<(), ClientError> {
        self.send(Method::POST, path, Some(body)).await.map(drop)
    }

    async fn post_empty(&self, path: &str) -> Result<(), ClientError> {
        self.send::<()>(Method::POST, path, None).await.map(drop)
    }

    pub async fn status(&self) -> Result<Status, ClientError> {
        self.get("/status").await
    }

    pub async fn jobs(&self) -> Result<Vec<JobInfo>, ClientError> {
        self.get("/jobs").await
    }

    pub async fn add(&self, new: &NewJob) -> Result<u64, ClientError> {
        let resp = self.send(Method::POST, "/jobs", Some(new)).await?;
        Ok(resp.json::<AddResponse>().await?.id)
    }

    pub async fn start(&self, ids: &[u64]) -> Result<(), ClientError> {
        self.post("/jobs/start", &Ids { ids: ids.to_vec() }).await
    }

    pub async fn stop(&self, ids: &[u64]) -> Result<(), ClientError> {
        self.post("/jobs/stop", &Ids { ids: ids.to_vec() }).await
    }

    pub async fn remove(&self, ids: &[u64]) -> Result<(), ClientError> {
        self.post("/jobs/remove", &Ids { ids: ids.to_vec() }).await
    }

    pub async fn set_priority(&self, ids: &[u64], priority: Priority) -> Result<(), ClientError> {
        let body = PriorityRequest {
            ids: ids.to_vec(),
            priority,
        };
        self.post("/jobs/priority", &body).await
    }

    pub async fn set_job_speed_limit(
        &self,
        ids: &[u64],
        bytes_per_sec: Option<u64>,
    ) -> Result<(), ClientError> {
        let body = SpeedLimitRequest {
            ids: ids.to_vec(),
            bytes_per_sec,
        };
        self.post("/jobs/speed-limit", &body).await
    }

    pub async fn start_all(&self) -> Result<(), ClientError> {
        self.post_empty("/queue/start-all").await
    }

    pub async fn pause_all(&self) -> Result<(), ClientError> {
        self.post_empty("/queue/pause-all").await
    }

    pub async fn stop_all(&self) -> Result<(), ClientError> {
        self.post_empty("/queue/stop-all").await
    }

    pub async fn settings(&self) -> Result<QueueSettings, ClientError> {
        self.get("/settings").await
    }

    pub async fn set_settings(&self, s: &QueueSettings) -> Result<QueueSettings, ClientError> {
        let resp = self.send(Method::PUT, "/settings", Some(s)).await?;
        Ok(resp.json().await?)
    }

    pub async fn servers(&self) -> Result<Vec<ServerStatus>, ClientError> {
        self.get("/servers").await
    }

    /// Pide al servicio que pare (guarda el progreso antes).
    pub async fn shutdown(&self) -> Result<(), ClientError> {
        self.post_empty("/shutdown").await
    }
}
