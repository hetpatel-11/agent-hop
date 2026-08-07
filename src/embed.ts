import * as ort from "onnxruntime-web";
import { Tokenizer } from "@huggingface/tokenizers";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const MODEL_DIR = join(homedir(), ".agentresume", "model");
const MODEL_URL = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx";
const TOKENIZER_URL = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
const TOKENIZER_CONFIG_URL = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer_config.json";

// onnxruntime-web's WASM backend defaults to using every available core for
// its internal thread pool. Earlier benchmarking found that extra threading
// doesn't actually speed up embedding here (already bottlenecked elsewhere),
// so there's no real tradeoff -- go all the way to 1 thread rather than
// guessing a fixed number like "2", which would still be meaningful
// contention on a low-core machine and an arbitrary guess on a high-core
// one. 1 needs no assumption about the machine at all.
ort.env.wasm.numThreads = 1;

let session: ort.InferenceSession | undefined;
let tokenizer: Tokenizer | undefined;

async function downloadFile(url: string, dest: string, onProgress?: (msg: string) => void): Promise<void> {
  onProgress?.(`Downloading ${dest.split("/").pop()}...`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Failed to download ${url}: ${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
}

/** Downloads the embedding model + tokenizer on first use (~23MB total),
 * cached under ~/.agentresume/model/ forever after. Every call after the
 * first is instant (cache hit). */
export async function ensureModel(onProgress?: (msg: string) => void): Promise<void> {
  mkdirSync(MODEL_DIR, { recursive: true });
  const modelPath = join(MODEL_DIR, "model.onnx");
  const tokenizerPath = join(MODEL_DIR, "tokenizer.json");
  const tokenizerConfigPath = join(MODEL_DIR, "tokenizer_config.json");

  if (!existsSync(modelPath)) await downloadFile(MODEL_URL, modelPath, onProgress);
  if (!existsSync(tokenizerPath)) await downloadFile(TOKENIZER_URL, tokenizerPath, onProgress);
  if (!existsSync(tokenizerConfigPath)) await downloadFile(TOKENIZER_CONFIG_URL, tokenizerConfigPath, onProgress);

  if (!session) {
    onProgress?.("Loading embedding model...");
    session = await ort.InferenceSession.create(modelPath);
  }
  if (!tokenizer) {
    const tokenizerJson = JSON.parse(readFileSync(tokenizerPath, "utf-8"));
    const tokenizerConfig = JSON.parse(readFileSync(tokenizerConfigPath, "utf-8"));
    tokenizer = new Tokenizer(tokenizerJson, tokenizerConfig);
  }
}

/** Embeds text into a 384-dim, L2-normalized vector via mean pooling over
 * token embeddings (standard sentence-embedding technique for BERT-family
 * models like MiniLM). */
const MAX_TOKENS = 512; // MiniLM/BERT's hard position-embedding limit -- the
// runtime throws (not silently truncates) if you exceed it, confirmed via a
// real crash: "Attempting to broadcast axis... 512 by 525" once chunks got
// long enough to tokenize past this. Must truncate ourselves.

export async function embedText(text: string): Promise<Float32Array> {
  if (!session || !tokenizer) throw new Error("embedText: call ensureModel() first");

  const encoded = tokenizer.encode(text);
  const n = Math.min(encoded.ids.length, MAX_TOKENS);
  const ids = encoded.ids.slice(0, n);
  const attnMask = encoded.attention_mask.slice(0, n);
  const feeds = {
    input_ids: new ort.Tensor("int64", BigInt64Array.from(ids.map(BigInt)), [1, n]),
    attention_mask: new ort.Tensor("int64", BigInt64Array.from(attnMask.map(BigInt)), [1, n]),
    token_type_ids: new ort.Tensor("int64", BigInt64Array.from(new Array(n).fill(0n)), [1, n]),
  };
  const results = await session.run(feeds);
  const hidden = results.last_hidden_state;
  const dim = hidden.dims[2] as number;
  const data = hidden.data as Float32Array;

  const vec = new Float32Array(dim);
  let count = 0;
  for (let t = 0; t < n; t++) {
    if (encoded.attention_mask[t] === 0) continue;
    for (let d = 0; d < dim; d++) vec[d] += data[t * dim + d];
    count++;
  }
  for (let d = 0; d < dim; d++) vec[d] /= count || 1;

  let norm = 0;
  for (let d = 0; d < dim; d++) norm += vec[d] * vec[d];
  norm = Math.sqrt(norm) || 1;
  for (let d = 0; d < dim; d++) vec[d] /= norm;

  return vec;
}

/** Embeds many texts in one forward pass (padded to the batch's longest
 * sequence) instead of one call per text. Amortizes fixed per-call overhead
 * (tokenization, tensor construction, the JS/WASM boundary crossing) across
 * the whole batch -- meaningfully cheaper per-item than calling embedText()
 * in a loop, on top of and independent from cross-worker parallelism. */
export async function embedTextsBatch(texts: string[]): Promise<Float32Array[]> {
  if (!session || !tokenizer) throw new Error("embedTextsBatch: call ensureModel() first");
  if (texts.length === 0) return [];

  const encodedAll = texts.map((t) => {
    const enc = tokenizer!.encode(t);
    const n = Math.min(enc.ids.length, MAX_TOKENS);
    return { ids: enc.ids.slice(0, n), mask: enc.attention_mask.slice(0, n) };
  });
  const maxLen = Math.max(...encodedAll.map((e) => e.ids.length));
  const batchSize = texts.length;

  const ids = new BigInt64Array(batchSize * maxLen);
  const mask = new BigInt64Array(batchSize * maxLen);
  const types = new BigInt64Array(batchSize * maxLen);
  encodedAll.forEach((e, i) => {
    for (let j = 0; j < maxLen; j++) {
      ids[i * maxLen + j] = j < e.ids.length ? BigInt(e.ids[j]) : 0n;
      mask[i * maxLen + j] = j < e.mask.length ? BigInt(e.mask[j]) : 0n;
    }
  });

  const feeds = {
    input_ids: new ort.Tensor("int64", ids, [batchSize, maxLen]),
    attention_mask: new ort.Tensor("int64", mask, [batchSize, maxLen]),
    token_type_ids: new ort.Tensor("int64", types, [batchSize, maxLen]),
  };
  const results = await session.run(feeds);
  const hidden = results.last_hidden_state;
  const dim = hidden.dims[2] as number;
  const data = hidden.data as Float32Array;

  const out: Float32Array[] = [];
  for (let b = 0; b < batchSize; b++) {
    const vec = new Float32Array(dim);
    let count = 0;
    for (let t = 0; t < maxLen; t++) {
      if (mask[b * maxLen + t] === 0n) continue;
      const base = (b * maxLen + t) * dim;
      for (let d = 0; d < dim; d++) vec[d] += data[base + d];
      count++;
    }
    for (let d = 0; d < dim; d++) vec[d] /= count || 1;

    let norm = 0;
    for (let d = 0; d < dim; d++) norm += vec[d] * vec[d];
    norm = Math.sqrt(norm) || 1;
    for (let d = 0; d < dim; d++) vec[d] /= norm;

    out.push(vec);
  }
  return out;
}

export function cosineSimilarity(a: Float32Array, b: Float32Array): number {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i] * b[i];
  return s;
}
