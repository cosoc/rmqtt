#[macro_use]
extern crate serde;
#[macro_use]
extern crate serde_json;

mod config;

use async_trait::async_trait;
use config::PluginConfig;
use crossbeam::channel::{bounded, Receiver, Sender};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmqtt::broker::error::MqttError;
use rmqtt::{
    broker::hook::{self, Handler, HookResult, Parameter, Register, ReturnType},
    broker::types::{ConnectInfo, QoSEx, MQTT_LEVEL_5},
    plugin::Plugin,
    Result, Runtime, Topic,
};

#[inline]
pub async fn init<N: Into<String>, D: Into<String>>(
    runtime: &'static Runtime,
    name: N,
    descr: D,
    default_startup: bool,
) -> Result<()> {
    runtime
        .plugins
        .register(Box::new(WebHookPlugin::new(runtime, name.into(), descr.into()).await?), default_startup)
        .await?;
    Ok(())
}

struct WebHookPlugin {
    runtime: &'static Runtime,
    name: String,
    descr: String,
    register: Box<dyn Register>,

    cfg: Arc<RwLock<PluginConfig>>,
    tx: Arc<RwLock<Sender<Message>>>,
    processings: Arc<AtomicIsize>,
}

impl WebHookPlugin {
    #[inline]
    async fn new(runtime: &'static Runtime, name: String, descr: String) -> Result<Self> {
        let cfg = Arc::new(RwLock::new(
            runtime
                .settings
                .plugins
                .load_config::<PluginConfig>(&name)
                .map_err(|e| MqttError::from(e.to_string()))?,
        ));
        log::debug!("{} WebHookPlugin cfg: {:?}", name, cfg.read());
        let processings = Arc::new(AtomicIsize::new(0));
        let tx = Arc::new(RwLock::new(Self::start(runtime, cfg.clone(), processings.clone())));
        let register = runtime.extends.hook_mgr().await.register();
        Ok(Self { runtime, name, descr, register, cfg, tx, processings })
    }

    fn start(
        _runtime: &'static Runtime,
        cfg: Arc<RwLock<PluginConfig>>,
        processings: Arc<AtomicIsize>,
    ) -> Sender<Message> {
        let (tx, rx): (Sender<Message>, Receiver<Message>) = bounded(cfg.read().async_queue_capacity);
        let _child = std::thread::Builder::new().name("web-hook".to_string()).spawn(move || {
            log::info!("start web-hook async worker.");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(cfg.read().worker_threads)
                .thread_name("web-hook-worker")
                .thread_stack_size(4 * 1024 * 1024)
                .build()
                .unwrap();

            let runner = async {
                loop {
                    let cfg = cfg.clone();
                    let processings = processings.clone();
                    match rx.recv() {
                        Ok(msg) => {
                            log::trace!("received web-hook Message: {:?}", msg);
                            match msg {
                                Message::Body(typ, topic, data) => {
                                    processings.fetch_add(1, Ordering::SeqCst);
                                    tokio::task::spawn(async move {
                                        if let Err(e) =
                                            WebHookHandler::handle(cfg.clone(), typ, topic, data).await
                                        {
                                            log::error!("send web hook message error, {:?}", e);
                                        }
                                        processings.fetch_sub(1, Ordering::SeqCst);
                                    });
                                }
                                Message::Exit => {
                                    log::debug!("Is Message::Exit message ...");
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("web hook message channel abnormal, {:?}", e);
                        }
                    }
                }
            };
            rt.block_on(runner);
            log::info!("exit web-hook async worker.");
        });
        tx
    }
}

#[async_trait]
impl Plugin for WebHookPlugin {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        log::info!("{} init", self.name);

        let register = |typ: hook::Type| {
            self.register.add(
                typ,
                Box::new(WebHookHandler {
                    //cfg: self.cfg.clone(),
                    tx: self.tx.clone(),
                }),
            );
        };

        register(hook::Type::SessionCreated);
        register(hook::Type::SessionTerminated);
        register(hook::Type::SessionSubscribed);
        register(hook::Type::SessionUnsubscribed);

        register(hook::Type::ClientConnect);
        register(hook::Type::ClientConnack);
        register(hook::Type::ClientConnected);
        register(hook::Type::ClientDisconnected);
        register(hook::Type::ClientSubscribe);
        register(hook::Type::ClientUnsubscribe);

        register(hook::Type::MessagePublish);
        register(hook::Type::MessageDelivered);
        register(hook::Type::MessageAcked);
        register(hook::Type::MessageDropped);

        Ok(())
    }

    #[inline]
    fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    async fn start(&mut self) -> Result<()> {
        log::info!("{} start", self.name);
        self.register.start();
        Ok(())
    }

    #[inline]
    async fn stop(&mut self) -> Result<bool> {
        log::info!("{} stop", self.name);
        self.register.stop();
        Ok(true)
    }

    #[inline]
    fn version(&self) -> &str {
        "0.1.1"
    }

    #[inline]
    fn descr(&self) -> &str {
        &self.descr
    }

    #[inline]
    fn attrs(&self) -> serde_json::Value {
        json!({
            "queue_len": self.tx.read().len(),
            "active_tasks": self.processings.load(Ordering::SeqCst)
        })
    }

    #[inline]
    async fn load_config(&mut self) -> Result<()> {
        let new_cfg = self.runtime.settings.plugins.load_config::<PluginConfig>(&self.name)?;
        let cfg = { self.cfg.read().clone() };
        if cfg.worker_threads != new_cfg.worker_threads
            || cfg.async_queue_capacity != new_cfg.async_queue_capacity
        {
            let new_cfg = Arc::new(RwLock::new(new_cfg));
            //restart
            let new_tx = Self::start(self.runtime, new_cfg.clone(), self.processings.clone());
            if let Err(e) = self.tx.read().send_timeout(Message::Exit, std::time::Duration::from_secs(3)) {
                log::error!("restart web-hook failed, {:?}", e);
                return Err(MqttError::Error(Box::new(e)));
            }
            self.cfg = new_cfg;
            *self.tx.write() = new_tx;
        } else {
            *self.cfg.write() = new_cfg;
        }
        log::debug!("load_config ok,  {:?}", self.cfg);
        Ok(())
    }

    #[inline]
    fn get_config(&self) -> Result<serde_json::Value> {
        self.cfg.read().to_json()
    }
}

lazy_static::lazy_static! {
    static ref  HTTP_CLIENT: reqwest::Client = {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap()
    };
}

#[derive(Debug)]
pub enum Message {
    Body(hook::Type, Option<Topic>, serde_json::Value),
    Exit,
}

struct WebHookHandler {
    //cfg: Arc<RwLock<PluginConfig>>,
    tx: Arc<RwLock<Sender<Message>>>,
}

impl WebHookHandler {
    async fn handle(
        cfg: Arc<RwLock<PluginConfig>>,
        typ: hook::Type,
        topic: Option<Topic>,
        body: serde_json::Value,
    ) -> Result<()> {
        let (timeout, default_urls) = {
            let cfg = cfg.read();
            (cfg.http_timeout, cfg.http_urls.clone())
        };

        let http_requests = if let Some(rules) = cfg.read().rules.get(&typ) {
            //get action and urls
            let action_urls = rules.iter().filter_map(|r| {
                let is_allowed = if let Some(topic) = &topic {
                    if let Some((rule_topics, _)) = &r.topics {
                        rule_topics.is_matches(topic)
                    } else {
                        true
                    }
                } else {
                    true
                };

                if is_allowed {
                    let urls = if r.urls.is_empty() { &default_urls } else { &r.urls };
                    if urls.is_empty() {
                        None
                    } else {
                        Some((&r.action, urls))
                    }
                } else {
                    None
                }
            });

            //build http send futures
            let mut http_requests = Vec::new();
            for (action, urls) in action_urls {
                let mut new_body = body.clone();
                if let Some(obj) = new_body.as_object_mut() {
                    obj.insert("action".into(), serde_json::Value::String(action.clone()));
                }
                if urls.len() == 1 {
                    log::debug!("action: {}, url: {}", action, urls[0]);
                    http_requests.push(Self::http_request(urls[0].clone(), new_body, timeout));
                } else {
                    for url in urls {
                        log::debug!("action: {}, url: {}", action, url);
                        http_requests.push(Self::http_request(url.clone(), new_body.clone(), timeout));
                    }
                }
            }

            Some(http_requests)
        } else {
            None
        };

        //send http_requests
        if let Some(http_requests) = http_requests {
            log::debug!("http_requests length: {}", http_requests.len());
            let _ = futures::future::join_all(http_requests).await;
        }

        Ok(())
    }

    async fn http_request(url: String, body: serde_json::Value, timeout: Duration) {
        log::debug!("http_request, timeout: {:?}, url: {}, body: {}", timeout, url, body);
        match HTTP_CLIENT
            .clone()
            .request(reqwest::Method::POST, &url)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
        {
            Err(e) => {
                log::error!("url:{:?}, error:{:?}", url, e);
            }
            Ok(resp) => {
                if !resp.status().is_success() {
                    log::warn!("response status is not OK, url:{:?}, response:{:?}", url, resp);
                }
            }
        }
    }
}

trait ToBody {
    fn to_body(&self) -> serde_json::Value;
}

impl ToBody for ConnectInfo {
    fn to_body(&self) -> serde_json::Value {
        match self {
            ConnectInfo::V3(id, conn_info) => {
                json!({
                    "node": id.node(),
                    "ipaddress": id.remote_addr,
                    "clientid": id.client_id,
                    "username": id.username,
                    "keepalive": conn_info.keep_alive,
                    "proto_ver": conn_info.protocol.level(),
                    "clean_session": conn_info.clean_session,
                })
            }
            ConnectInfo::V5(id, conn_info) => {
                json!({
                    "node": id.node(),
                    "ipaddress": id.remote_addr,
                    "clientid": id.client_id,
                    "username": id.username,
                    "keepalive": conn_info.keep_alive,
                    "proto_ver": MQTT_LEVEL_5,
                    "clean_start": conn_info.clean_start,
                })
            }
        }
    }
}

#[async_trait]
impl Handler for WebHookHandler {
    async fn hook(&mut self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        let typ = param.get_type();

        let bodys = match param {
            Parameter::ClientConnect(conn_info) => {
                vec![(None, conn_info.to_body())]
            }
            Parameter::ClientConnack(conn_info, conn_ack) => {
                let mut body = conn_info.to_body();
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("conn_ack".into(), serde_json::Value::String(conn_ack.reason().to_string()));
                }
                vec![(None, body)]
            }

            Parameter::ClientConnected(_session, client) => {
                let mut body = client.connect_info.to_body();
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "connected_at".into(),
                        serde_json::Value::Number(serde_json::Number::from(client.connected_at)),
                    );
                    obj.insert("session_present".into(), serde_json::Value::Bool(client.session_present));
                }
                vec![(None, body)]
            }

            Parameter::ClientDisconnected(_session, client, reason) => {
                let body = json!({
                    "node": client.id.node(),
                    "ipaddress": client.id.remote_addr,
                    "clientid": client.id.client_id,
                    "username": client.id.username,
                    "disconnected_at": client.disconnected_at(),
                    "reason": reason
                });
                vec![(None, body)]
            }

            Parameter::ClientSubscribe(_session, client, subscribe) => {
                let mut bodys = Vec::new();
                for (topic, qos) in subscribe.topic_filters().drain(..) {
                    let body = json!({
                        "node": client.id.node(),
                        "ipaddress": client.id.remote_addr,
                        "clientid": client.id.client_id,
                        "username": client.id.username,
                        "topic": topic.to_string(),
                        "opts": json!({
                            "qos": qos.value()
                        }),
                    });
                    bodys.push((Some(topic), body));
                }
                bodys
            }

            Parameter::ClientUnsubscribe(_session, client, unsubscribe) => {
                let mut bodys = Vec::new();
                for topic in unsubscribe.topic_filters().drain(..) {
                    let body = json!({
                        "node": client.id.node(),
                        "ipaddress": client.id.remote_addr,
                        "clientid": client.id.client_id,
                        "username": client.id.username,
                        "topic": topic.to_string(),
                    });
                    bodys.push((Some(topic), body));
                }
                bodys
            }

            Parameter::SessionSubscribed(_session, client, subscribed) => {
                let (topic, qos) = subscribed.topic_filter();
                let body = json!({
                    "node": client.id.node(),
                    "ipaddress": client.id.remote_addr,
                    "clientid": client.id.client_id,
                    "username": client.id.username,
                    "topic": topic.to_string(),
                    "opts": json!({
                        "qos": qos.value()
                    }),
                });
                vec![(Some(topic), body)]
            }

            Parameter::SessionUnsubscribed(_session, client, unsubscribed) => {
                let topic = unsubscribed.topic_filter();
                let body = json!({
                    "node": client.id.node(),
                    "ipaddress": client.id.remote_addr,
                    "clientid": client.id.client_id,
                    "username": client.id.username,
                    "topic": topic.to_string(),
                });
                vec![(Some(topic), body)]
            }

            Parameter::SessionCreated(session, client) => {
                let body = json!({
                    "node": client.id.node(),
                    "ipaddress": client.id.remote_addr,
                    "clientid": client.id.client_id,
                    "username": client.id.username,
                    "created_at": session.created_at,
                });
                vec![(None, body)]
            }

            Parameter::SessionTerminated(_session, client, reason) => {
                let body = json!({
                    "node": client.id.node(),
                    "ipaddress": client.id.remote_addr,
                    "clientid": client.id.client_id,
                    "username": client.id.username,
                    "reason": reason
                });
                vec![(None, body)]
            }

            Parameter::MessagePublish(_session, client, publish) => {
                let topic = publish.topic().clone();
                let body = json!({
                    "from": client.id.to_json(),
                    "dup": publish.dup(),
                    "retain": publish.retain(),
                    "qos": publish.qos().value(),
                    "topic": topic.to_string(),
                    "packet_id": publish.packet_id(),
                    "payload": base64::encode(publish.payload()),
                    "ts": publish.create_time(),
                });
                vec![(Some(topic), body)]
            }

            Parameter::MessageDelivered(_session, client, from, publish) => {
                let topic = publish.topic().clone();
                let body = json!({
                    "to": client.id.to_json(),
                    "from": from.to_json(),
                    "dup": publish.dup(),
                    "retain": publish.retain(),
                    "qos": publish.qos().value(),
                    "topic": topic.to_string(),
                    "packet_id": publish.packet_id(),
                    "payload": base64::encode(publish.payload()),
                    "ts": chrono::Local::now().timestamp_millis(),
                });
                vec![(Some(topic), body)]
            }

            Parameter::MessageAcked(_session, client, from, publish) => {
                let topic = publish.topic().clone();
                let body = json!({
                    "to": client.id.to_json(),
                    "from": from.to_json(),
                    "dup": publish.dup(),
                    "retain": publish.retain(),
                    "qos": publish.qos().value(),
                    "topic": topic.to_string(),
                    "packet_id": publish.packet_id(),
                    "payload": base64::encode(publish.payload()),
                    "ts": chrono::Local::now().timestamp_millis(),
                });
                vec![(Some(topic), body)]
            }

            Parameter::MessageDropped(_session, _client, to, from, publish, reason) => {
                let body = json!({
                    "to": to.as_ref().map(|to|to.to_json()),
                    "from": from.to_json(),
                    "dup": publish.dup(),
                    "retain": publish.retain(),
                    "qos": publish.qos().value(),
                    "topic": publish.topic().to_string(),
                    "packet_id": publish.packet_id(),
                    "payload": base64::encode(publish.payload()),
                    "reason": reason,
                    "ts": chrono::Local::now().timestamp_millis(),
                });
                vec![(None, body)]
            }
            _ => {
                log::error!("parameter is: {:?}", param);
                Vec::new()
            }
        };

        log::debug!("bodys: {:?}", bodys);

        if !bodys.is_empty() {
            for (topic, body) in bodys {
                if let Err(e) = self.tx.read().try_send(Message::Body(typ, topic, body)) {
                    log::warn!("web-hook send error, typ: {:?}, {:?}", typ, e);
                }
            }
        }

        (true, acc)
    }
}