//! The embedder (`specs/knowledge.md` §1.6): `bge-small-en-v1.5`, a 12-layer
//! BERT with CLS pooling and L2 normalisation, run on burn 0.20 with the
//! backend as a type parameter — wgpu for a build, ndarray on a serving box
//! without a GPU. Composed from burn's own attention, feed-forward, layer
//! norm and embedding modules rather than its transformer encoder, so the
//! layer norm epsilon can be BERT's 1e-12 and the post-norm order is
//! explicit. Weights load from the Hugging Face safetensors through
//! burn-store with the key remapping in [`load`].
//!
//! Probe 2026-09-21 against the Python model: worst cosine 0.999999 over
//! twenty sentences, tokenizer ids identical (measured).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use burn::module::Module;
use burn::nn::attention::{MhaInput, MultiHeadAttention, MultiHeadAttentionConfig};
use burn::nn::transformer::{PositionWiseFeedForward, PositionWiseFeedForwardConfig};
use burn::nn::{Embedding, EmbeddingConfig, LayerNorm, LayerNormConfig};
use burn::prelude::*;
use burn_store::{KeyRemapper, ModuleSnapshot, PyTorchToBurnAdapter, SafetensorsStore};
use rill_store::Hash;
use tokenizers::{PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub use burn::backend::{NdArray, Wgpu};

pub const MODEL_NAME: &str = "bge-small-en-v1.5";
pub const DIM: usize = 384;
const HEADS: usize = 12;
const LAYERS: usize = 12;
const FF: usize = 1536;
const VOCAB: usize = 30522;
const MAX_POS: usize = 512;
const EPS: f64 = 1e-12;
/// The model's context; longer inputs are truncated by the tokenizer.
pub const MAX_TOKENS: usize = 512;
/// bge's instruction for short queries against passages (its model card).
pub const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// What every consumer programs against; the backend is behind it.
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize {
        DIM
    }
    /// The identity a pack records: the model name and the safetensors hash.
    fn model_name(&self) -> &str;
    fn model_hash(&self) -> Hash;
    /// Unit vectors, one per input, in order.
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>>;
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Tokenizer(String),
    Weights(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Tokenizer(m) => write!(f, "tokenizer: {m}"),
            Error::Weights(m) => write!(f, "weights: {m}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Module, Debug)]
struct Embeddings<B: Backend> {
    word_embeddings: Embedding<B>,
    position_embeddings: Embedding<B>,
    token_type_embeddings: Embedding<B>,
    layer_norm: LayerNorm<B>,
}

#[derive(Module, Debug)]
struct Layer<B: Backend> {
    mha: MultiHeadAttention<B>,
    norm_1: LayerNorm<B>,
    pwff: PositionWiseFeedForward<B>,
    norm_2: LayerNorm<B>,
}

#[derive(Module, Debug)]
struct Bert<B: Backend> {
    embeddings: Embeddings<B>,
    layers: Vec<Layer<B>>,
}

impl<B: Backend> Bert<B> {
    fn init(device: &B::Device) -> Self {
        let norm = || LayerNormConfig::new(DIM).with_epsilon(EPS).init(device);
        let embeddings = Embeddings {
            word_embeddings: EmbeddingConfig::new(VOCAB, DIM).init(device),
            position_embeddings: EmbeddingConfig::new(MAX_POS, DIM).init(device),
            token_type_embeddings: EmbeddingConfig::new(2, DIM).init(device),
            layer_norm: norm(),
        };
        let layers = (0..LAYERS)
            .map(|_| Layer {
                mha: MultiHeadAttentionConfig::new(DIM, HEADS).with_dropout(0.0).init(device),
                norm_1: norm(),
                pwff: PositionWiseFeedForwardConfig::new(DIM, FF).with_dropout(0.0).init(device),
                norm_2: norm(),
            })
            .collect();
        Bert { embeddings, layers }
    }

    /// CLS-pooled, L2-normalised: `[batch, DIM]`.
    fn forward(&self, ids: Tensor<B, 2, Int>, mask_pad: Tensor<B, 2, Bool>) -> Tensor<B, 2> {
        let [b, s] = ids.dims();
        let device = ids.device();
        let positions = Tensor::<B, 1, Int>::arange(0..s as i64, &device).reshape([1, s]).expand([b, s]);
        let types = Tensor::<B, 2, Int>::zeros([b, s], &device);
        let e = &self.embeddings;
        let mut x = e.word_embeddings.forward(ids) + e.position_embeddings.forward(positions) + e.token_type_embeddings.forward(types);
        x = e.layer_norm.forward(x);
        for layer in &self.layers {
            let attn = layer.mha.forward(MhaInput::self_attn(x.clone()).mask_pad(mask_pad.clone())).context;
            x = layer.norm_1.forward(x + attn);
            let ff = layer.pwff.forward(x.clone());
            x = layer.norm_2.forward(x + ff);
        }
        let cls = x.slice([0..b, 0..1]).reshape([b, DIM]);
        let norm = cls.clone().powf_scalar(2.0).sum_dim(1).sqrt();
        cls / norm
    }
}

/// Load the Hugging Face checkpoint into the module, remapping its keys.
fn load<B: Backend>(model: Bert<B>, path: &Path) -> Result<Bert<B>, Error> {
    let remap = KeyRemapper::from_patterns(vec![
        (r"^encoder\.layer\.([0-9]+)", "layers.$1"),
        (r"attention\.self\.query", "mha.query"),
        (r"attention\.self\.key", "mha.key"),
        (r"attention\.self\.value", "mha.value"),
        (r"attention\.output\.dense", "mha.output"),
        (r"attention\.output\.LayerNorm", "norm_1"),
        (r"intermediate\.dense", "pwff.linear_inner"),
        (r"(layers\.[0-9]+)\.output\.dense", "$1.pwff.linear_outer"),
        (r"(layers\.[0-9]+)\.output\.LayerNorm", "$1.norm_2"),
        (r"embeddings\.LayerNorm", "embeddings.layer_norm"),
    ])
    .map_err(|e| Error::Weights(e.to_string()))?;
    let mut store = SafetensorsStore::from_file(path).with_from_adapter(PyTorchToBurnAdapter).remap(remap).allow_partial(true);
    let mut model = model;
    let result = model.load_from(&mut store).map_err(|e| Error::Weights(e.to_string()))?;
    if !result.missing.is_empty() {
        return Err(Error::Weights(format!("{} tensors missing, first {:?}", result.missing.len(), result.missing.first())));
    }
    if !result.errors.is_empty() {
        return Err(Error::Weights(format!("{:?}", result.errors[0])));
    }
    Ok(model)
}

/// A loaded model on one backend. Burn parameters are lazily
/// materialised (`OnceCell`), which is not `Sync`, so the model sits
/// behind a mutex: one forward pass at a time, which is how a server
/// embeds queries and how a build streams batches anyway.
pub struct Bge<B: Backend> {
    model: Mutex<Bert<B>>,
    device: B::Device,
    tokenizer: Tokenizer,
    hash: Hash,
    batch: usize,
}

impl<B: Backend> Bge<B> {
    /// `dir` holds `model.safetensors` and `tokenizer.json`.
    pub fn open(dir: &Path, device: B::Device) -> Result<Bge<B>, Error> {
        let weights: PathBuf = dir.join("model.safetensors");
        let bytes = std::fs::read(&weights).map_err(Error::Io)?;
        let hash = Hash::of(&bytes);
        drop(bytes);
        let model = load(Bert::<B>::init(&device), &weights)?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| Error::Tokenizer(e.to_string()))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".into(),
        }));
        tokenizer
            .with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(Bge { model: Mutex::new(model), device, tokenizer, hash, batch: 64 })
    }

    /// Inputs per forward pass; larger is faster on a GPU up to memory.
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }

    fn embed_batch(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        let encs = self.tokenizer.encode_batch(texts.to_vec(), true).expect("tokenize");
        let (b, s) = (encs.len(), encs[0].get_ids().len());
        let mut ids = Vec::with_capacity(b * s);
        let mut mask = Vec::with_capacity(b * s);
        for e in &encs {
            ids.extend(e.get_ids().iter().map(|&x| x as i64));
            mask.extend(e.get_attention_mask().iter().map(|&m| m == 0));
        }
        let ids_t = Tensor::<B, 2, Int>::from_data(TensorData::new(ids, [b, s]), &self.device);
        let mask_t = Tensor::<B, 2, Bool>::from_data(TensorData::new(mask, [b, s]), &self.device);
        let out: Vec<f32> = self.model.lock().expect("model lock").forward(ids_t, mask_t).into_data().to_vec().expect("read back");
        out.as_chunks::<DIM>().0.iter().map(|c| c.to_vec()).collect()
    }
}

impl<B: Backend> Embedder for Bge<B> {
    fn model_name(&self) -> &str {
        MODEL_NAME
    }

    fn model_hash(&self) -> Hash {
        self.hash
    }

    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch) {
            out.extend(self.embed_batch(chunk));
        }
        out
    }
}

/// The wgpu-backed model on the default adapter.
pub fn open_gpu(dir: &Path) -> Result<Bge<Wgpu>, Error> {
    Bge::<Wgpu>::open(dir, Default::default())
}

/// The CPU model.
pub fn open_cpu(dir: &Path) -> Result<Bge<NdArray>, Error> {
    Bge::<NdArray>::open(dir, Default::default())
}

/// A model on whichever backend the name asks for.
pub fn open(dir: &Path, backend: &str) -> Result<Box<dyn Embedder>, Error> {
    match backend {
        "cpu" => Ok(Box::new(open_cpu(dir)?)),
        _ => Ok(Box::new(open_gpu(dir)?)),
    }
}
