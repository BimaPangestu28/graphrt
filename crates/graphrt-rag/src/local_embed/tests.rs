//! Tests run against a tiny randomly initialised BERT written to a temp
//! directory, so they need no network and exercise the real file formats.

use std::path::{Path, PathBuf};

use candle_core::Device;
use candle_nn::{VarBuilder, VarMap};
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};

use super::*;
use crate::types::Embedder;

const WORDS: [&str; 12] = ["[PAD]", "[UNK]", "[CLS]", "[SEP]", "a", "b", "c", "d", "e", "f", "cat", "dog"];

/// A temp directory holding a tiny model, removed when dropped.
struct TinyModel(PathBuf);

impl TinyModel {
    fn create(name: &str, model_type: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-local-embed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = serde_json::json!({
            "vocab_size": WORDS.len(), "hidden_size": 8, "num_hidden_layers": 1, "num_attention_heads": 2,
            "intermediate_size": 16, "hidden_act": "gelu", "hidden_dropout_prob": 0.0,
            "max_position_embeddings": 32, "type_vocab_size": 2, "initializer_range": 0.02,
            "layer_norm_eps": 1e-12, "pad_token_id": 0, "model_type": model_type
        });
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        write_tokenizer(&dir);
        let bert_config: BertConfig = serde_json::from_value(config).unwrap();
        let vars = VarMap::new();
        BertModel::load(VarBuilder::from_varmap(&vars, DTYPE, &Device::Cpu), &bert_config).unwrap();
        vars.save(dir.join("model.safetensors")).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TinyModel {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_tokenizer(dir: &Path) {
    let vocab: serde_json::Map<String, serde_json::Value> =
        WORDS.iter().enumerate().map(|(i, w)| ((*w).to_owned(), serde_json::Value::from(i))).collect();
    let tokenizer = serde_json::json!({
        "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": { "type": "Whitespace" },
        "post_processor": { "type": "BertProcessing", "sep": ["[SEP]", 3], "cls": ["[CLS]", 2] },
        "decoder": null,
        "model": { "type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]" }
    });
    std::fs::write(dir.join("tokenizer.json"), tokenizer.to_string()).unwrap();
}

fn embedder(model: &TinyModel, configure: impl FnOnce(&mut LocalEmbedderConfig)) -> LocalEmbedder {
    let mut config = LocalEmbedderConfig::from_dir(model.path());
    configure(&mut config);
    LocalEmbedder::new(config).unwrap()
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(|t| (*t).to_owned()).collect()
}

fn assert_close(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert!((x - y).abs() < 1e-5, "{a:?} != {b:?}");
    }
}

#[test]
fn embeds_unit_vectors_of_the_model_width() {
    let model = TinyModel::create("width", "bert");
    let embedder = embedder(&model, |_| {});
    assert_eq!(embedder.dimensions(), None, "unknown before the first load");
    let vectors = futures::executor::block_on(embedder.embed_documents(&texts(&["a b c", "cat dog"]))).unwrap();
    assert_eq!(vectors.len(), 2);
    for v in &vectors {
        assert_eq!(v.len(), 8);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
    }
    assert_eq!(embedder.dimensions(), Some(8));
}

#[test]
fn padding_does_not_change_a_text_vector() {
    // A short text batched with a long one is padded; mean pooling must ignore the padding.
    for pooling in [Pooling::Mean, Pooling::Cls] {
        let model = TinyModel::create(&format!("padding-{pooling:?}"), "bert");
        let embedder = embedder(&model, |c| c.pooling = pooling);
        let alone = futures::executor::block_on(embedder.embed_query("a b")).unwrap();
        let batched =
            futures::executor::block_on(embedder.embed_documents(&texts(&["a b", "a b c d e f cat dog"]))).unwrap();
        assert_close(&alone, &batched[0]);
    }
}

#[test]
fn batch_size_does_not_change_results() {
    let model = TinyModel::create("batches", "bert");
    let inputs = texts(&["a", "b c", "cat", "dog d e", "f"]);
    let one_by_one = embedder(&model, |c| c.batch_size = 1);
    let all_at_once = embedder(&model, |c| c.batch_size = 16);
    let first = futures::executor::block_on(one_by_one.embed_documents(&inputs)).unwrap();
    let second = futures::executor::block_on(all_at_once.embed_documents(&inputs)).unwrap();
    for (a, b) in first.iter().zip(&second) {
        assert_close(a, b);
    }
}

#[test]
fn prefixes_reach_the_model() {
    let model = TinyModel::create("prefix", "bert");
    let plain = embedder(&model, |_| {});
    let prefixed = embedder(&model, |c| c.query_prefix = "dog ".into());
    let a = futures::executor::block_on(plain.embed_query("cat")).unwrap();
    let b = futures::executor::block_on(prefixed.embed_query("cat")).unwrap();
    let c = futures::executor::block_on(plain.embed_query("dog cat")).unwrap();
    assert!(a.iter().zip(&b).any(|(x, y)| (x - y).abs() > 1e-6), "prefix changed nothing");
    assert_close(&b, &c);
}

#[test]
fn missing_model_directory_is_reported() {
    let embedder = LocalEmbedder::new(LocalEmbedderConfig::from_dir("/nonexistent/model")).unwrap();
    let error = futures::executor::block_on(embedder.embed_documents(&[])).unwrap_err();
    assert_eq!(error.kind(), "model");
    assert!(error.to_string().contains("missing config.json"), "{error}");
}

#[test]
fn rejects_other_architectures_clearly() {
    let model = TinyModel::create("arch", "xlm-roberta");
    let embedder = embedder(&model, |_| {});
    let error = futures::executor::block_on(embedder.embed_query("a")).unwrap_err();
    assert!(error.to_string().contains("unsupported model_type `xlm-roberta`"), "{error}");
}

#[test]
fn config_validation_blocks_path_traversal_and_plain_http() {
    let bad = |configure: fn(&mut LocalEmbedderConfig)| {
        let mut config = LocalEmbedderConfig::default();
        configure(&mut config);
        LocalEmbedder::new(config).unwrap_err().to_string()
    };
    assert!(bad(|c| c.model = "../../etc".into()).contains("`model`"));
    assert!(bad(|c| c.model = "no-owner".into()).contains("`model`"));
    assert!(bad(|c| c.model = "owner/name/extra".into()).contains("`model`"));
    assert!(bad(|c| c.revision = "..".into()).contains("`revision`"));
    assert!(bad(|c| c.revision = "a/b".into()).contains("`revision`"));
    assert!(bad(|c| c.endpoint = "http://mirror.example".into()).contains("https"));
    assert!(bad(|c| c.batch_size = 0).contains("positive"));
    assert!(LocalEmbedder::new(LocalEmbedderConfig::new("BAAI/bge-small-en-v1.5")).is_ok());
    let loopback = LocalEmbedderConfig { endpoint: "http://127.0.0.1:8080".into(), ..LocalEmbedderConfig::default() };
    assert!(LocalEmbedder::new(loopback).is_ok());
}

#[test]
fn normalize_handles_zero_vectors() {
    let mut zero = vec![0.0, 0.0];
    model::normalize(&mut zero);
    assert_eq!(zero, vec![0.0, 0.0]);
    let mut v = vec![3.0, 4.0];
    model::normalize(&mut v);
    assert_close(&v, &[0.6, 0.8]);
}

/// Downloads a real model; run with `cargo test -p graphrt-rag --features local-embed -- --ignored`.
#[cfg(feature = "http")]
#[tokio::test]
#[ignore = "downloads ~90 MB from huggingface.co"]
async fn real_model_ranks_related_sentences_higher() {
    let embedder = LocalEmbedder::new(LocalEmbedderConfig::default()).unwrap();
    let query = embedder.embed_query("How many days of paid leave do I get?").await.unwrap();
    let docs = embedder
        .embed_documents(&texts(&[
            "Every employee gets 24 days of paid annual leave per year.",
            "Expenses below 500 USD are approved by your manager.",
        ]))
        .await
        .unwrap();
    assert_eq!(query.len(), 384);
    let related = crate::memory::cosine(&query, &docs[0]);
    let unrelated = crate::memory::cosine(&query, &docs[1]);
    eprintln!("related {related:.3} vs unrelated {unrelated:.3}");
    assert!(related > unrelated + 0.2, "related {related} vs unrelated {unrelated}");
}

/// Multilingual model with the e5 prefixes, on Indonesian text (~470 MB download).
#[cfg(feature = "http")]
#[tokio::test]
#[ignore = "downloads ~470 MB from huggingface.co"]
async fn multilingual_model_handles_indonesian() {
    let mut config = LocalEmbedderConfig::new("intfloat/multilingual-e5-small");
    config.query_prefix = "query: ".into();
    config.document_prefix = "passage: ".into();
    let embedder = LocalEmbedder::new(config).unwrap();
    let query = embedder.embed_query("Berapa hari cuti tahunan yang saya dapat?").await.unwrap();
    let docs = embedder
        .embed_documents(&texts(&[
            "Setiap karyawan mendapat 24 hari cuti tahunan berbayar per tahun.",
            "Pengeluaran di bawah 500 USD disetujui oleh manajer Anda.",
        ]))
        .await
        .unwrap();
    assert_eq!(query.len(), 384);
    let related = crate::memory::cosine(&query, &docs[0]);
    let unrelated = crate::memory::cosine(&query, &docs[1]);
    eprintln!("related {related:.3} vs unrelated {unrelated:.3}");
    assert!(related > unrelated, "related {related} vs unrelated {unrelated}");
}
