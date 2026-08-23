//! Port of src/embed.ts -- downloads the same MiniLM ONNX model + tokenizer
//! HuggingFace assets on first use, cached under ~/.agent-hop/model/
//! forever after, then embeds text into 384-dim L2-normalized vectors via
//! mean pooling (standard sentence-embedding technique for BERT-family
//! models). Faithful, literal port -- comments carried over from the TS
//! source since they document real, hard-won behavior.

use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokenizers::Tokenizer;

const MODEL_URL: &str = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx";
const TOKENIZER_URL: &str = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
const TOKENIZER_CONFIG_URL: &str = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer_config.json";

fn model_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("model")
}

/// Pinned ONNX Runtime release. `ort` is built with `load-dynamic` (see
/// Cargo.toml's comment on why: `libghostty-vt`, the terminal engine
/// backing the TUI, statically links a large enough native library on its
/// own that a second one -- the ~20MB+ of object code ONNX Runtime's
/// default static-linking mode would otherwise embed -- pushes the binary
/// past a hard ARM64 relative-addressing limit (`ADRP/imm12` can't reach
/// `___dso_handle` from OrtApis code anymore, confirmed live as a real
/// linker failure). `load-dynamic` means `ort` never embeds ONNX Runtime's
/// code at all; it loads a `.dylib`/`.so`/`.dll` at runtime instead, found
/// via `ORT_DYLIB_PATH`. Downloaded and cached under
/// ~/.agent-hop/onnxruntime/ the same way the embedding model itself
/// already is (see `ensure_model` below) -- once per version, forever
/// after.
const ORT_VERSION: &str = "1.22.0";

fn onnxruntime_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("onnxruntime").join(ORT_VERSION)
}

/// (release asset name without extension, path to the dylib/so/dll inside
/// the extracted archive, whether the asset is a .zip instead of .tar.gz).
fn onnxruntime_asset() -> anyhow::Result<(&'static str, &'static str, bool)> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("onnxruntime-osx-arm64", "lib/libonnxruntime.dylib", false),
        ("macos", "x86_64") => ("onnxruntime-osx-x86_64", "lib/libonnxruntime.dylib", false),
        ("linux", "x86_64") => ("onnxruntime-linux-x64", "lib/libonnxruntime.so", false),
        ("linux", "aarch64") => ("onnxruntime-linux-aarch64", "lib/libonnxruntime.so", false),
        ("windows", "x86_64") => ("onnxruntime-win-x64", "lib/onnxruntime.dll", true),
        ("windows", "aarch64") => ("onnxruntime-win-arm64", "lib/onnxruntime.dll", true),
        (os, arch) => anyhow::bail!("no prebuilt ONNX Runtime available for {os}/{arch}"),
    })
}

fn extract_tar_gz(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    let tar = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(tar);
    archive.unpack(dest)?;
    Ok(())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    archive.extract(dest)?;
    Ok(())
}

/// Downloads and extracts the official ONNX Runtime release for the
/// current platform on first use, cached under
/// ~/.agent-hop/onnxruntime/<version>/ forever after. Returns the path to
/// the extracted dynamic library, for `ort::init_from`.
async fn ensure_onnxruntime_dylib(on_progress: &(impl Fn(&str) + Sync)) -> anyhow::Result<PathBuf> {
    let (asset_name, lib_rel_path, is_zip) = onnxruntime_asset()?;
    let dir = onnxruntime_dir();
    let extracted_root = dir.join(format!("{asset_name}-{ORT_VERSION}"));
    let lib_path = extracted_root.join(lib_rel_path);

    if lib_path.exists() {
        return Ok(lib_path);
    }

    std::fs::create_dir_all(&dir)?;
    let ext = if is_zip { "zip" } else { "tgz" };
    let url = format!("https://github.com/microsoft/onnxruntime/releases/download/v{ORT_VERSION}/{asset_name}-{ORT_VERSION}.{ext}");
    on_progress("Downloading ONNX Runtime...");
    let bytes = reqwest::get(&url).await?.error_for_status()?.bytes().await?;

    if is_zip {
        extract_zip(&bytes, &dir)?;
    } else {
        extract_tar_gz(&bytes, &dir)?;
    }

    anyhow::ensure!(lib_path.exists(), "ONNX Runtime archive extracted but expected library not found at {}", lib_path.display());
    Ok(lib_path)
}

// ort's Error<R> carries the failed resource (e.g. the SessionBuilder
// itself) for some calls, which contains raw FFI pointers that aren't
// Send+Sync -- anyhow's blanket `?` conversion requires Send+Sync, so
// every ort error is stringified through Display instead of propagated
// via `?` directly.
fn ort_err<R>(e: ort::Error<R>) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

async fn download_file(url: &str, dest: &std::path::Path, on_progress: &(impl Fn(&str) + Sync)) -> anyhow::Result<()> {
    on_progress(&format!("Downloading {}...", dest.file_name().unwrap_or_default().to_string_lossy()));
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

/// Downloads the embedding model + tokenizer on first use (~23MB total),
/// cached under ~/.agent-hop/model/ forever after. Every call after the
/// first is instant (cache hit).
pub async fn ensure_model(on_progress: impl Fn(&str) + Sync) -> anyhow::Result<()> {
    let dir = model_dir();
    std::fs::create_dir_all(&dir)?;
    let model_path = dir.join("model.onnx");
    let tokenizer_path = dir.join("tokenizer.json");
    let tokenizer_config_path = dir.join("tokenizer_config.json");

    if !model_path.exists() {
        download_file(MODEL_URL, &model_path, &on_progress).await?;
    }
    if !tokenizer_path.exists() {
        download_file(TOKENIZER_URL, &tokenizer_path, &on_progress).await?;
    }
    if !tokenizer_config_path.exists() {
        download_file(TOKENIZER_CONFIG_URL, &tokenizer_config_path, &on_progress).await?;
    }

    if SESSION.get().is_none() {
        // Must happen before the first `Session::builder()` call anywhere
        // in the process -- `ort`'s API loader is a lazily-initialized
        // global that locks in whichever dylib path it finds (or its
        // platform-default guess) the first time any `ort` API is touched.
        let dylib_path = ensure_onnxruntime_dylib(&on_progress).await?;
        // SAFETY: single-threaded at this point in startup (called once,
        // guarded by `SESSION.get().is_none()` above, before any other
        // thread has a reason to touch `ort`) -- `set_var` here can't race
        // with a read of the same variable from another thread.
        unsafe {
            std::env::set_var("ORT_DYLIB_PATH", &dylib_path);
        }

        on_progress("Loading embedding model...");
        let builder = Session::builder().map_err(ort_err)?;
        let mut builder = builder.with_optimization_level(GraphOptimizationLevel::Level3).map_err(ort_err)?;
        let session = builder.commit_from_file(&model_path).map_err(ort_err)?;
        let _ = SESSION.set(Mutex::new(session));
    }
    if TOKENIZER.get().is_none() {
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;
        let _ = TOKENIZER.set(tokenizer);
    }
    Ok(())
}

// MiniLM/BERT's hard position-embedding limit -- the runtime errors (not
// silently truncates) if you exceed it. Must truncate ourselves.
const MAX_TOKENS: usize = 512;

/// Embeds text into a 384-dim, L2-normalized vector via mean pooling over
/// token embeddings (standard sentence-embedding technique for BERT-family
/// models like MiniLM).
pub fn embed_text(text: &str) -> anyhow::Result<Vec<f32>> {
    let session_lock = SESSION.get().ok_or_else(|| anyhow::anyhow!("embed_text: call ensure_model() first"))?;
    let tokenizer = TOKENIZER.get().ok_or_else(|| anyhow::anyhow!("embed_text: call ensure_model() first"))?;
    let mut session = session_lock.lock().map_err(|_| anyhow::anyhow!("embedding session lock poisoned"))?;

    let encoding = tokenizer.encode(text, true).map_err(|e| anyhow::anyhow!("tokenize failed: {e}"))?;
    let ids = encoding.get_ids();
    let mask = encoding.get_attention_mask();
    let n = ids.len().min(MAX_TOKENS);

    let input_ids: Vec<i64> = ids[..n].iter().map(|&x| x as i64).collect();
    let attn_mask: Vec<i64> = mask[..n].iter().map(|&x| x as i64).collect();
    let token_type_ids: Vec<i64> = vec![0; n];

    let input_ids_tensor = Tensor::from_array(([1usize, n], input_ids)).map_err(ort_err)?;
    let attn_mask_tensor = Tensor::from_array(([1usize, n], attn_mask.clone())).map_err(ort_err)?;
    let token_type_tensor = Tensor::from_array(([1usize, n], token_type_ids)).map_err(ort_err)?;

    let outputs = session
        .run(ort::inputs![
            "input_ids" => input_ids_tensor,
            "attention_mask" => attn_mask_tensor,
            "token_type_ids" => token_type_tensor,
        ])
        .map_err(ort_err)?;
    let (shape, data) = outputs["last_hidden_state"].try_extract_tensor::<f32>().map_err(ort_err)?;
    let dim = shape[2] as usize;

    let mut vec = vec![0f32; dim];
    let mut count = 0f32;
    for t in 0..n {
        if attn_mask[t] == 0 {
            continue;
        }
        for d in 0..dim {
            vec[d] += data[t * dim + d];
        }
        count += 1.0;
    }
    if count > 0.0 {
        for v in vec.iter_mut() {
            *v /= count;
        }
    }

    let mut norm: f32 = vec.iter().map(|v| v * v).sum();
    norm = norm.sqrt();
    if norm == 0.0 {
        norm = 1.0;
    }
    for v in vec.iter_mut() {
        *v /= norm;
    }

    Ok(vec)
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real end-to-end check of the ort/tokenizers integration (the single
    /// riskiest new dependency in this port): downloads the actual MiniLM
    /// model on first run, embeds real sentences, and confirms the
    /// resulting vectors behave like genuine semantic embeddings -- two
    /// sentences about the same topic should be more similar to each other
    /// than either is to an unrelated sentence. Marked #[ignore] since it
    /// needs network access for the one-time model download; run
    /// explicitly with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn embeddings_are_semantically_meaningful() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(ensure_model(|msg| println!("{msg}"))).unwrap();

        let a = embed_text("How do I fix a bug in my Rust code?").unwrap();
        let b = embed_text("Debugging an error in my Rust program").unwrap();
        let c = embed_text("What's the best recipe for chocolate cake?").unwrap();

        assert_eq!(a.len(), 384);
        let sim_related = cosine_similarity(&a, &b);
        let sim_unrelated = cosine_similarity(&a, &c);
        println!("sim(related) = {sim_related}, sim(unrelated) = {sim_unrelated}");
        assert!(sim_related > sim_unrelated, "related sentences should score more similar than unrelated ones");
        assert!(sim_related > 0.5, "genuinely related sentences should score well above 0.5 cosine similarity");
    }
}
