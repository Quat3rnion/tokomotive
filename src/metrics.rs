//! Prometheus metrics for throughput, tokens and money.

use metrics::{counter, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::db::NewEntry;

pub fn install() -> anyhow::Result<PrometheusHandle> {
    let buckets = [
        0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    ];
    Ok(PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), &buckets)?
        .install_recorder()?)
}

pub fn record(user: &str, e: &NewEntry) {
    let model = e
        .model
        .clone()
        .or_else(|| e.requested_model.clone())
        .unwrap_or_default();
    let base = [
        ("provider", e.provider.clone()),
        ("model", model),
        ("user", user.to_owned()),
    ];
    let mut with_status = base.to_vec();
    with_status.push(("status", e.status.to_string()));
    counter!("llm_requests_total", &with_status).increment(1);

    let u = &e.usage;
    for (class, n) in [
        ("input", u.input),
        ("output", u.output),
        ("cache_read", u.cache_read),
        ("cache_write_5m", u.cache_write_5m),
        ("cache_write_1h", u.cache_write_1h),
        ("image_input", u.image_input),
        ("image_output", u.image_output),
    ] {
        if n > 0 {
            let mut l = base.to_vec();
            l.push(("class", class.to_owned()));
            counter!("llm_tokens_total", &l).increment(n as u64);
        }
    }
    counter!("llm_cost_micros_total", &base).increment(e.quote.cost_micros.max(0) as u64);
    counter!("llm_price_micros_total", &base).increment(e.quote.price_micros.max(0) as u64);
    if e.quote.unpriced {
        counter!("llm_unpriced_requests_total", &base).increment(1);
    }
    if e.estimated {
        counter!("llm_estimated_requests_total", &base).increment(1);
    }
    histogram!("llm_request_duration_seconds", &base).record(e.latency_ms as f64 / 1000.0);
    if let Some(t) = e.ttft_ms {
        histogram!("llm_time_to_first_token_seconds", &base).record(t as f64 / 1000.0);
    }
}
