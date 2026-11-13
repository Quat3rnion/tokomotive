use axum::{
    Json, Router,
    extract::Form,
    http::HeaderMap,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

pub struct MockOAuth {
    pub base: String,
    pub claims: Arc<Mutex<Value>>,
    pub requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
    pub forged: Arc<Mutex<bool>>,
    pub scope: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockOAuth {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockOAuth {
    pub async fn start() -> Self {
        static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
        let key = KEY.get_or_init(|| RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap());
        let encoding = EncodingKey::from_rsa_der(key.to_pkcs1_der().unwrap().as_bytes());
        let jwks = json!({"keys":[{"kty":"RSA","kid":"test","alg":"RS256","use":"sig","n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let claims = Arc::new(Mutex::new(
            json!({"iss":base,"aud":"issued-client","sub":"test-account","email":"op@example.test","nonce":"unset","exp":chrono::Utc::now().timestamp()+3600,"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}),
        ));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let forged = Arc::new(Mutex::new(false));
        let scope = Arc::new(Mutex::new("openid chatgpt.tokens.use.direct".to_owned()));
        let scope_response = scope.clone();
        let (c, r, f) = (claims.clone(), requests.clone(), forged.clone());
        let app=Router::new().route("/.well-known/jwks.json",get(move || {let jwks=jwks.clone();async move {Json(jwks)}}))
            .route("/v1/responses",post(|headers:HeaderMap| async move {
                assert_eq!(headers["authorization"],"Bearer chatgpt-access");
                Json(json!({"id":"response","model":"gpt-test","output":[],"usage":{"input_tokens":7,"output_tokens":2}}))
            }))
            .route("/api/accounts/oauth/token",post(move |Form(form):Form<HashMap<String,String>>| {
                let (c,r,f,encoding,scope)=(c.clone(),r.clone(),f.clone(),encoding.clone(),scope_response.clone());async move {
                    r.lock().unwrap().push(form);
                    let mut header=Header::new(Algorithm::RS256);header.kid=Some("test".into());
                    let mut signed=jsonwebtoken::encode(&header,&*c.lock().unwrap(),&encoding).unwrap();
                    if *f.lock().unwrap() {let (payload,sig)=signed.rsplit_once('.').unwrap();let mut bytes=URL_SAFE_NO_PAD.decode(sig).unwrap();bytes[0]^=1;signed=format!("{payload}.{}",URL_SAFE_NO_PAD.encode(bytes));}
                    Json(json!({"access_token":"chatgpt-access","refresh_token":"chatgpt-refresh","expires_in":3600,"scope":*scope.lock().unwrap(),"id_token":signed}))
                }
            }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            claims,
            requests,
            forged,
            scope,
            task,
        }
    }
}
