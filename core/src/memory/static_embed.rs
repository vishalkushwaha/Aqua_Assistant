//! A static embedding model, read by hand (card R3-06): a table with one vector for every word piece of a vocabulary, and a text is the average of the
//! vectors of its pieces. There is no network in it, no layers and no graphics card: a text is a handful of table look-ups, about a microsecond each.
//!
//! This reads the plain form of a Model2Vec model (the `potion-*` family), the three files of its folder:
//!
//! * `model.safetensors`: one tensor, `embeddings`, of shape `[pieces, numbers]` in `F32`, `F16` or `BF16`;
//! * `tokenizer.json`: the vocabulary and how a text is cut into pieces (read by the `tokenizers` crate Aqua already carries for the Laya gate);
//! * `config.json` (optional): `normalize` (default true: a text's vector is scaled to length 1).
//!
//! **What it refuses.** A model file that holds a `mapping` or `weights` tensor (a model with a reduced vocabulary or a weight per piece) is refused with a
//! reason instead of being read as if it were plain: the vectors would come out wrong without any error. A file with a value that is not a finite number,
//! a table that is not two-dimensional, or a size over [`MAX_FILE_BYTES`] is refused as well. Pieces the vocabulary marks unknown are dropped, as the
//! model's own library drops them; a piece whose number is past the end of the table is skipped, not an error.
//!
//! The file reader (the safetensors header) is the `safetensors` crate; nothing else is downloaded or executed from the model folder.

use super::embedding::EmbeddingProvider;
use safetensors::{Dtype, SafeTensors};
use std::path::Path;
use tokenizers::Tokenizer;

/// The largest file this reads (the 32M model is about 130 MB): a bigger one is refused before it is read.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Word pieces of one text that are used (the rest of a long text is left out), as in the model's own library.
pub const MAX_PIECES: usize = 512;
/// Characters of one text that are cut into pieces at all: a text longer than this has nothing more to say to a table of word vectors.
pub const MAX_CHARS: usize = 8_000;
/// The most numbers in a vector this accepts.
const MAX_DIM: usize = 4_096;

const MODEL_FILE: &str = "model.safetensors";
const TOKENIZER_FILE: &str = "tokenizer.json";
const CONFIG_FILE: &str = "config.json";

pub struct StaticEmbedder {
    name: String,
    tokenizer: Tokenizer,
    rows: usize,
    dim: usize,
    /// `rows * dim` numbers, one row after the other.
    table: Vec<f32>,
    normalize: bool,
    /// The number of the piece the vocabulary marks unknown, when it names one.
    unknown: Option<u32>,
}

impl StaticEmbedder {
    /// Reads the model in `dir`. The error names the file that is missing or wrong.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let read = |file: &str, required: bool| -> Result<Option<Vec<u8>>, String> {
            let path = dir.join(file);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    if meta.len() > MAX_FILE_BYTES {
                        return Err(format!("{} is {} bytes, over the {} this reads", path.display(), meta.len(), MAX_FILE_BYTES));
                    }
                    std::fs::read(&path).map(Some).map_err(|e| format!("{} could not be read: {e}", path.display()))
                }
                _ if required => Err(format!("{file} is not in {}", dir.display())),
                _ => Ok(None),
            }
        };
        let weights = read(MODEL_FILE, true)?.ok_or_else(|| format!("{MODEL_FILE} is not in {}", dir.display()))?;
        let tokens = read(TOKENIZER_FILE, true)?.ok_or_else(|| format!("{TOKENIZER_FILE} is not in {}", dir.display()))?;
        let config = read(CONFIG_FILE, false)?.map(|b| String::from_utf8_lossy(&b).into_owned());
        let name = dir.file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("static-embedding").to_string();
        Self::from_parts(&name, &tokens, &weights, config.as_deref())
    }

    /// The model from the contents of its files (what [`Self::load`] reads, and what a test writes itself).
    pub fn from_parts(name: &str, tokenizer_json: &[u8], safetensors_bytes: &[u8], config_json: Option<&str>) -> Result<Self, String> {
        let file = SafeTensors::deserialize(safetensors_bytes).map_err(|e| format!("{MODEL_FILE} is not a safetensors file: {e}"))?;
        let names: Vec<String> = file.names().into_iter().map(|n| n.to_string()).collect();
        for extra in ["mapping", "weights"] {
            if names.iter().any(|n| n == extra) {
                return Err(format!("{MODEL_FILE} holds a `{extra}` tensor (a model with a reduced vocabulary or a weight for each piece), which this reader does not read: use a plain potion model, whose only tensor is `embeddings`"));
            }
        }
        let table = file.tensor("embeddings").map_err(|_| format!("{MODEL_FILE} has no `embeddings` tensor (it has: {})", names.join(", ")))?;
        let shape = table.shape();
        if shape.len() != 2 || shape[0] == 0 || shape[1] == 0 {
            return Err(format!("`embeddings` must be a table of rows and columns, and its shape is {shape:?}"));
        }
        let (rows, dim) = (shape[0], shape[1]);
        if dim > MAX_DIM {
            return Err(format!("`embeddings` has {dim} numbers in a row, over the {MAX_DIM} this reads"));
        }
        let numbers = decode(table.dtype(), table.data(), rows * dim)?;

        let normalize = match config_json {
            Some(text) => serde_json::from_str::<serde_json::Value>(text).map_err(|e| format!("{CONFIG_FILE} is not valid JSON: {e}"))?.get("normalize").and_then(|v| v.as_bool()).unwrap_or(true),
            None => true,
        };
        let mut tokenizer = Tokenizer::from_bytes(tokenizer_json).map_err(|e| format!("{TOKENIZER_FILE} could not be read: {e}"))?;
        // Neither cuts nor pads: a text is as many pieces as it is, and the table is looked up for each.
        let _ = tokenizer.with_truncation(None);
        tokenizer.with_padding(None);
        let unknown = serde_json::from_slice::<serde_json::Value>(tokenizer_json).ok().and_then(|v| v["model"]["unk_token"].as_str().and_then(|t| tokenizer.token_to_id(t)));
        Ok(Self { name: name.to_string(), tokenizer, rows, dim, table: numbers, normalize, unknown })
    }

    /// The vector of a text: the average of the vectors of its pieces, scaled to length 1 unless the model says not to. All zeros when none of its pieces is known.
    pub fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut sum = vec![0.0f32; self.dim];
        let text: String = text.chars().take(MAX_CHARS).collect();
        let Ok(encoding) = self.tokenizer.encode(text.as_str(), false) else { return sum };
        let mut used = 0usize;
        for &id in encoding.get_ids() {
            if Some(id) == self.unknown || id as usize >= self.rows {
                continue;
            }
            if used == MAX_PIECES {
                break;
            }
            let row = &self.table[id as usize * self.dim..(id as usize + 1) * self.dim];
            for (s, r) in sum.iter_mut().zip(row) {
                *s += r;
            }
            used += 1;
        }
        if used == 0 {
            return sum;
        }
        for s in sum.iter_mut() {
            *s /= used as f32;
        }
        if self.normalize {
            let norm = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for s in sum.iter_mut() {
                    *s /= norm;
                }
            }
        }
        sum
    }
}

impl EmbeddingProvider for StaticEmbedder {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_text(text)
    }
}

/// The numbers of a tensor as `f32`, refusing any that is not finite.
fn decode(dtype: Dtype, data: &[u8], count: usize) -> Result<Vec<f32>, String> {
    let width = match dtype {
        Dtype::F32 => 4,
        Dtype::F16 | Dtype::BF16 => 2,
        other => return Err(format!("`embeddings` is stored as {other:?}, and this reads F32, F16 and BF16")),
    };
    if data.len() != count * width {
        return Err(format!("`embeddings` holds {} bytes, and its shape needs {}", data.len(), count * width));
    }
    let mut out = Vec::with_capacity(count);
    for chunk in data.chunks_exact(width) {
        let value = match dtype {
            Dtype::F32 => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            Dtype::F16 => f16_to_f32(u16::from_le_bytes([chunk[0], chunk[1]])),
            _ => f32::from_bits((u16::from_le_bytes([chunk[0], chunk[1]]) as u32) << 16),
        };
        if !value.is_finite() {
            return Err("`embeddings` holds a value that is not a finite number".to_string());
        }
        out.push(value);
    }
    Ok(out)
}

/// IEEE half precision to single precision, by hand (the crate that does it is not a dependency of this one).
fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exponent = ((h >> 10) & 0x1f) as u32;
    let fraction = (h & 0x3ff) as u32;
    let bits = if exponent == 0 {
        if fraction == 0 {
            sign << 31
        } else {
            // A subnormal half is a normal single: move its leading one to the place a single keeps it.
            let (mut e, mut f) = (113u32, fraction);
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            (sign << 31) | (e << 23) | ((f & 0x3ff) << 13)
        }
    } else if exponent == 31 {
        (sign << 31) | 0x7f80_0000 | (fraction << 13)
    } else {
        (sign << 31) | ((exponent + 112) << 23) | (fraction << 13)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A tiny model that tests write themselves: a vocabulary of a few words and a table of four numbers a word, in which "sister", "sibling" and
    //! "brother" point the same way, "colour" and "green" another, "pune" a third and "cricket" a fourth.
    use serde_json::json;
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::WhitespaceSplit;
    use tokenizers::Tokenizer;

    /// `(word, piece number, row of the table)`. Piece 0 is the unknown piece; "ghost" has a number past the end of the table on purpose.
    pub const WORDS: &[(&str, u32, [f32; 4])] = &[
        ("[UNK]", 0, [0.0, 0.0, 0.0, 0.0]),
        ("sister", 1, [1.0, 0.25, 0.0, 0.0]),
        ("sibling", 2, [0.5, 0.0, 0.0, 0.0]),
        ("brother", 3, [1.0, 0.0, 0.5, 0.0]),
        ("name", 4, [0.25, 0.0, 0.0, 0.125]),
        ("pune", 5, [0.0, 0.0, 1.0, 0.0]),
        ("cricket", 6, [0.0, 0.0, 0.0, 1.0]),
        ("colour", 7, [0.0, 1.0, 0.0, 0.0]),
        ("green", 8, [0.0, 0.5, 0.25, 0.0]),
        ("ghost", 99, [0.0, 0.0, 0.0, 0.0]),
    ];
    pub const ROWS: usize = 9;

    pub fn tokenizer_json() -> Vec<u8> {
        let vocab: serde_json::Map<String, serde_json::Value> = WORDS.iter().map(|(w, id, _)| (w.to_string(), json!(id))).collect();
        // From text and not from a `Value`: the model's reader takes the word "WordLevel" as a borrowed string, which a `Value` cannot give.
        let model: WordLevel = serde_json::from_str(&json!({"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}).to_string()).expect("a word-level vocabulary");
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(WhitespaceSplit));
        tokenizer.to_string(false).expect("the tokenizer is written").into_bytes()
    }

    /// A safetensors file, written by hand: eight bytes with the length of the header, the header (JSON), and the data. Tensors in the order of their names.
    pub fn safetensors(tensors: &[(&str, &str, Vec<usize>, Vec<u8>)]) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        let (mut offset, mut data) = (0usize, Vec::new());
        for (name, dtype, shape, bytes) in tensors {
            header.insert(name.to_string(), json!({"dtype": dtype, "shape": shape, "data_offsets": [offset, offset + bytes.len()]}));
            data.extend_from_slice(bytes);
            offset += bytes.len();
        }
        let mut text = serde_json::to_string(&serde_json::Value::Object(header)).unwrap().into_bytes();
        while text.len() % 8 != 0 {
            text.push(b' ');
        }
        let mut out = (text.len() as u64).to_le_bytes().to_vec();
        out.extend(text);
        out.extend(data);
        out
    }

    /// The table of [`WORDS`] (the rows of the pieces that are in it) as `F32`.
    pub fn table_f32() -> Vec<u8> {
        WORDS.iter().filter(|(_, id, _)| (*id as usize) < ROWS).flat_map(|(_, _, row)| row.iter().flat_map(|x| x.to_le_bytes())).collect()
    }

    pub fn model_f32() -> Vec<u8> {
        safetensors(&[("embeddings", "F32", vec![ROWS, 4], table_f32())])
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::memory::embedding::cosine;

    fn model() -> StaticEmbedder {
        StaticEmbedder::from_parts("tiny", &tokenizer_json(), &model_f32(), None).expect("the tiny model loads")
    }

    #[test]
    fn a_model_the_test_wrote_is_read_and_a_text_is_the_unit_length_average_of_its_pieces() {
        let m = model();
        assert_eq!((m.dim(), m.name()), (4, "tiny".to_string()));
        let one = m.embed("sister");
        assert!((one.iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs() < 1e-5, "{one:?}");
        // "sister" is [1, 0.25, 0, 0] and "name" [0.25, 0, 0, 0.125]: the text is their average, scaled to length 1.
        let two = m.embed("sister name");
        let want = [1.25f32 / 2.0, 0.25 / 2.0, 0.0, 0.125 / 2.0];
        let norm = want.iter().map(|x| x * x).sum::<f32>().sqrt();
        for (got, w) in two.iter().zip(want) {
            assert!((got - w / norm).abs() < 1e-5, "{two:?}");
        }
        assert_eq!(m.embed_many(&["sister", "pune"]).len(), 2);
    }

    #[test]
    fn a_text_in_other_words_is_closer_to_the_fact_than_an_unrelated_one() {
        let m = model();
        let fact = m.embed("sister name");
        let paraphrase = m.embed("sibling name");
        let unrelated = m.embed("cricket");
        assert!(cosine(&fact, &paraphrase) > 0.9, "{}", cosine(&fact, &paraphrase));
        assert!(cosine(&fact, &unrelated) < 0.2, "{}", cosine(&fact, &unrelated));
        assert!(cosine(&m.embed("sibling"), &m.embed("brother")) > cosine(&m.embed("sibling"), &m.embed("pune")));
    }

    #[test]
    fn unknown_words_and_a_piece_past_the_end_of_the_table_are_left_out_not_an_error() {
        let m = model();
        assert_eq!(m.embed("zzz qqq"), vec![0.0; 4], "nothing the model knows is all zeros");
        assert_eq!(m.embed(""), vec![0.0; 4]);
        assert_eq!(m.embed("sister zzz"), m.embed("sister"), "an unknown word does not pull the average toward zero");
        // "ghost" is piece 99 in a table of 9 rows.
        assert_eq!(m.embed("sister ghost"), m.embed("sister"));
    }

    #[test]
    fn half_and_brain_floats_are_read_like_full_ones() {
        // Values that are the same in all three forms: 1.0 (0x3C00 / 0x3F80), 0.5 (0x3800 / 0x3F00), -2.0 (0xC000 / 0xC000), 0.0.
        let rows: [[f32; 2]; 3] = [[1.0, 0.5], [-2.0, 0.0], [0.5, 1.0]];
        let f32_bytes: Vec<u8> = rows.iter().flatten().flat_map(|x| x.to_le_bytes()).collect();
        let half = |x: f32| -> u16 {
            match x {
                v if v == 1.0 => 0x3C00,
                v if v == 0.5 => 0x3800,
                v if v == -2.0 => 0xC000,
                _ => 0,
            }
        };
        let f16_bytes: Vec<u8> = rows.iter().flatten().flat_map(|x| half(*x).to_le_bytes()).collect();
        let bf16_bytes: Vec<u8> = rows.iter().flatten().flat_map(|x| ((x.to_bits() >> 16) as u16).to_le_bytes()).collect();
        let vocab = serde_json::json!({"type": "WordLevel", "vocab": {"[UNK]": 0, "a": 1, "b": 2}, "unk_token": "[UNK]"});
        let mut t = Tokenizer::new(serde_json::from_str::<tokenizers::models::wordlevel::WordLevel>(&vocab.to_string()).unwrap());
        t.with_pre_tokenizer(Some(tokenizers::pre_tokenizers::whitespace::WhitespaceSplit));
        let tok = t.to_string(false).unwrap().into_bytes();
        let make = |dtype: &str, bytes: Vec<u8>| StaticEmbedder::from_parts("t", &tok, &safetensors(&[("embeddings", dtype, vec![3, 2], bytes)]), Some(r#"{"normalize": false}"#)).unwrap();
        let (a, b, c) = (make("F32", f32_bytes), make("F16", f16_bytes), make("BF16", bf16_bytes));
        // "a b" is the average of rows 1 and 2: [(-2 + 0.5) / 2, (0 + 1) / 2]; not scaled, because the config says so.
        for m in [&a, &b, &c] {
            assert_eq!(m.embed("a b"), vec![-0.75, 0.5]);
        }
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x7C00), f32::INFINITY);
        assert!((f16_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12, "the smallest subnormal half");
        assert!((f16_to_f32(0x03FF) - 6.097_555_2e-5).abs() < 1e-10, "the largest subnormal half");
    }

    #[test]
    fn a_file_that_is_not_a_plain_model_is_refused_with_a_reason() {
        let tok = tokenizer_json();
        let reason = |bytes: Vec<u8>| StaticEmbedder::from_parts("x", &tok, &bytes, None).err().expect("refused");
        // A mapping or a weights tensor: the vectors would be wrong without any error, so it is refused.
        let with_mapping = safetensors(&[("embeddings", "F32", vec![ROWS, 4], table_f32()), ("mapping", "I32", vec![2], vec![0u8; 8])]);
        assert!(reason(with_mapping).contains("mapping"));
        let with_weights = safetensors(&[("embeddings", "F32", vec![ROWS, 4], table_f32()), ("weights", "F32", vec![ROWS], vec![0u8; ROWS * 4])]);
        assert!(reason(with_weights).contains("weights"));
        // No `embeddings` at all: the names it has are listed.
        let other = safetensors(&[("vectors", "F32", vec![ROWS, 4], table_f32())]);
        let why = reason(other);
        assert!(why.contains("embeddings") && why.contains("vectors"), "{why}");
        // A table that is not two-dimensional.
        assert!(reason(safetensors(&[("embeddings", "F32", vec![ROWS * 4], table_f32())])).contains("shape"));
        // A value that is not a number.
        let mut nan = table_f32();
        nan[0..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(reason(safetensors(&[("embeddings", "F32", vec![ROWS, 4], nan)])).contains("finite"));
        // A kind of number this does not read.
        assert!(reason(safetensors(&[("embeddings", "I32", vec![ROWS, 4], vec![0u8; ROWS * 16])])).contains("F32, F16 and BF16"));
        // Not a safetensors file at all, and one cut short.
        assert!(reason(b"this is not a model".to_vec()).contains("safetensors"));
        let whole = model_f32();
        assert!(reason(whole[..whole.len() - 8].to_vec()).contains("safetensors"));
        // A tokenizer that is not one.
        let err = StaticEmbedder::from_parts("x", b"{}", &model_f32(), None).err().expect("refused");
        assert!(err.contains("tokenizer.json"), "{err}");
        // A config that is not JSON.
        let err = StaticEmbedder::from_parts("x", &tok, &model_f32(), Some("not json")).err().expect("refused");
        assert!(err.contains("config.json"), "{err}");
    }

    #[test]
    fn a_folder_is_read_by_its_three_file_names_and_the_one_that_is_missing_is_named() {
        let dir = std::env::temp_dir().join(format!("aqua_static_embed_{}", std::process::id())).join("tiny-model");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.safetensors"), model_f32()).unwrap();
        let why = StaticEmbedder::load(&dir).err().expect("no tokenizer yet");
        assert!(why.contains("tokenizer.json"), "{why}");
        std::fs::write(dir.join("tokenizer.json"), tokenizer_json()).unwrap();
        let m = StaticEmbedder::load(&dir).expect("the config is optional");
        assert_eq!(m.name(), "tiny-model", "named after the folder");
        assert!(m.embed("sister")[0] > 0.9);
        std::fs::write(dir.join("config.json"), r#"{"normalize": false, "model_type": "model2vec"}"#).unwrap();
        let raw = StaticEmbedder::load(&dir).unwrap().embed("sister");
        assert!((raw[0] - 1.0).abs() < 1e-6 && (raw[1] - 0.25).abs() < 1e-6, "not scaled: {raw:?}");
        std::fs::remove_file(dir.join("model.safetensors")).unwrap();
        assert!(StaticEmbedder::load(&dir).err().unwrap().contains("model.safetensors"));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
