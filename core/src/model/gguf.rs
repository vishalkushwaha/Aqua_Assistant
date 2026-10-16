//! What a GGUF model file says about itself, read from its header without loading the model (card OA9-03; task PR-04 of the OpenCode audit).
//!
//! A GGUF file starts with the marker `GGUF`, a version, the number of tensors and the number of key-value pairs, and then the pairs: the architecture, the trained context length,
//! the number of layers, the width of a layer, the heads, how the weights were quantised, the tokenizer (an array of the whole vocabulary, which is most of the header), and often the
//! chat template. Everything Aqua used to know about a model it knew from the file name. This reads the header instead, so that a different model (a 4B, a 14B, a model with fewer
//! layers) is described by what it is and not by what it is called.
//!
//! **The reader is made for a file nobody has vouched for.** It never executes or maps the file, it reads forward only, and every count and length is bounded before anything is allocated
//! for it: at most [`MAX_KV`] pairs and [`MAX_TENSORS`] tensors (the same ceilings the import check of `llama_cli::check_gguf_header` already uses), no string longer than [`MAX_STRING`]
//! (1 MiB), no array longer than [`MAX_ARRAY`] elements, no header larger than [`MAX_HEADER_BYTES`], arrays nested at most [`MAX_DEPTH`] deep. What is not needed (the vocabulary, the
//! merges, the scores) is skipped by reading and throwing away, never kept. A file that breaks a bound, ends in the middle of a value or is not a GGUF file at all comes back as a
//! [`GgufError`] whose `Display` is one plain sentence, which is what the import route shows the person.
//!
//! It reads **versions 2 and 3** (version 1 used 32-bit counts and the model server no longer reads it either) in little-endian byte order, which is what every file in use has; a file
//! written big-endian is named as such and refused.
//!
//! What this does not do: it does not read the tensor table, so the weight of one layer is estimated from the file size ([`GgufInfo::bytes_per_layer`]), and it does not check that the
//! tensors are where the header says. A header that parses is not proof that the model loads; it keeps out a file that only has the name and a file that is a parser trap.

use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// Most key-value pairs a header may announce. A model has a few hundred; a count beyond this is not a count.
pub const MAX_KV: u64 = 100_000;
/// Most tensors a header may announce (the model's own tensors are a few thousand at the most).
pub const MAX_TENSORS: u64 = 1_000_000;
/// The longest string the reader will take (a key, a name, a chat template): 1 MiB.
pub const MAX_STRING: u64 = 1 << 20;
/// The longest array, in elements. The largest vocabularies in use are about a quarter of a million tokens, and their merges about as many again.
pub const MAX_ARRAY: u64 = 16_000_000;
/// The most bytes of header the reader goes through before it gives up (a real header is a few megabytes, nearly all of it the tokenizer).
pub const MAX_HEADER_BYTES: u64 = 256 << 20;
/// How deep arrays may hold arrays.
pub const MAX_DEPTH: u32 = 4;
/// The longest key. (The format allows 65,535 bytes; the keys that matter are under 64.)
const MAX_KEY: u64 = 4096;
/// The start of the file: marker, version, tensor count, pair count.
const FIXED_HEAD_BYTES: u64 = 24;
/// The most elements of a numeric array that is read (the heads of each layer) and not skipped.
const MAX_READ_ARRAY: u64 = 4096;

/// Why a file could not be read as a GGUF header. `Display` is one plain sentence with no path in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgufError {
    /// The file could not be opened or read (the system's words).
    Unreadable(String),
    /// Shorter than the start of a GGUF file.
    TooShort,
    /// Does not start with the marker `GGUF`.
    NotGguf,
    /// Written big-endian (the version reads as a huge number).
    BigEndian,
    /// A version the model server does not read.
    Version(u32),
    /// A count that is not a count.
    Counts { tensors: u64, pairs: u64 },
    /// The file ended in the middle of the named part.
    Truncated(&'static str),
    /// A string that claims more than [`MAX_STRING`] bytes.
    StringTooLong { what: &'static str, len: u64 },
    /// An array that claims more than [`MAX_ARRAY`] elements.
    ArrayTooLong(u64),
    /// A value of a type the format does not have.
    ValueType(u32),
    /// More header than [`MAX_HEADER_BYTES`].
    TooLarge,
    /// Arrays nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl std::fmt::Display for GgufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GgufError::Unreadable(why) => write!(f, "the file could not be read: {why}"),
            GgufError::TooShort => f.write_str("the file is too short to be a GGUF model file"),
            GgufError::NotGguf => f.write_str("the file is not a GGUF model file: it does not start with the GGUF marker"),
            GgufError::BigEndian => f.write_str("the file is a big-endian GGUF file, which the model server does not read"),
            GgufError::Version(v) => write!(f, "the file is GGUF version {v}, which the model server does not read (versions 2 and 3 are read)"),
            GgufError::Counts { tensors, pairs } => write!(f, "the file does not have a sound GGUF header (it claims {tensors} tensors and {pairs} settings)"),
            GgufError::Truncated(what) => write!(f, "the file ends in the middle of its header (in {what}), so it is damaged or not finished downloading"),
            GgufError::StringTooLong { what, len } => write!(f, "the file does not have a sound GGUF header ({what} claims {len} bytes)"),
            GgufError::ArrayTooLong(n) => write!(f, "the file does not have a sound GGUF header (a list claims {n} entries)"),
            GgufError::ValueType(t) => write!(f, "the file has a setting of type {t}, which GGUF does not have"),
            GgufError::TooLarge => f.write_str("the header of the file is larger than any model's, so it is refused"),
            GgufError::TooDeep => f.write_str("the file has lists inside lists deeper than any model's header"),
        }
    }
}

impl std::error::Error for GgufError {}

/// What the header says about the model. Every number a header may leave out is an `Option`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GgufInfo {
    /// 2 or 3.
    pub version: u32,
    pub tensor_count: u64,
    pub pair_count: u64,
    /// The length of the file (0 when the header was read from memory).
    pub file_bytes: u64,
    /// `general.architecture` ("qwen3", "llama", "gemma3"...), empty when the header does not say.
    pub architecture: String,
    /// `general.name`.
    pub name: Option<String>,
    /// `general.size_label` ("8B", "30B-A3B").
    pub size_label: Option<String>,
    /// `general.file_type`: how the weights were quantised, as llama.cpp numbers it.
    pub file_type: Option<u32>,
    /// The name of that quantisation ("Q4_K_M"), when the number is one that is known.
    pub quant: Option<String>,
    /// `<architecture>.context_length`: the longest context the model was trained for.
    pub context_length: Option<u64>,
    /// `<architecture>.block_count`: how many layers.
    pub block_count: Option<u32>,
    /// `<architecture>.embedding_length`: the width of a layer.
    pub embedding_length: Option<u32>,
    /// `<architecture>.attention.head_count` (the most of any layer, when it is given per layer).
    pub head_count: Option<u32>,
    /// `<architecture>.attention.head_count_kv`: the heads whose keys and values the cache holds.
    pub head_count_kv: Option<u32>,
    /// `<architecture>.attention.key_length` and `value_length`, when the architecture does not derive them from the width and the heads.
    pub key_length: Option<u32>,
    pub value_length: Option<u32>,
    /// `<architecture>.expert_count`: more than 0 is a mixture of experts, whose weight per layer is not even.
    pub expert_count: Option<u32>,
    /// How many tokens the vocabulary has (the length of `tokenizer.ggml.tokens`).
    pub vocab_size: Option<u64>,
    /// Does the file carry a chat template (`tokenizer.chat_template`)?
    pub has_chat_template: bool,
    /// A file that is one part of a model split over several: this part's number (from 0) and how many parts there are.
    pub split: Option<(u32, u32)>,
}

impl GgufInfo {
    /// The width of the keys and values the cache holds for one token of one layer, in elements: `head_count_kv * (key_length + value_length)`, with the lengths taken from the
    /// width and the heads when the header does not give them. `None` when the header does not give what is needed.
    pub fn kv_elements_per_token_per_layer(&self) -> Option<u64> {
        let heads_kv = u64::from(self.head_count_kv.or(self.head_count)?);
        let derived = || Some(u64::from(self.embedding_length?) / u64::from(self.head_count.filter(|h| *h > 0)?));
        let key = self.key_length.map(u64::from).or_else(derived)?;
        let value = self.value_length.map(u64::from).unwrap_or(key);
        Some(heads_kv * (key + value))
    }

    /// The bytes the cache takes for `layers` layers at a window of `window` tokens when each element is `element_bytes` (2 for the usual 16-bit cache). An estimate: it counts
    /// every layer as one that has attention, which is not so for models that mix other kinds of layers in.
    pub fn kv_bytes(&self, layers: u32, window: u32, element_bytes: u64) -> Option<u64> {
        Some(self.kv_elements_per_token_per_layer()? * u64::from(layers) * u64::from(window) * element_bytes)
    }

    /// The weight of one layer in bytes, estimated as the file's size over the layers plus one (the embedding and the output take about one layer's share). `None` when the file's
    /// size or the layer count is not known. **An estimate**, and a rough one for a mixture of experts.
    pub fn bytes_per_layer(&self) -> Option<u64> {
        let layers = u64::from(self.block_count?);
        (self.file_bytes > 0).then(|| self.file_bytes / (layers + 1))
    }

    /// Is this a mixture of experts?
    pub fn is_moe(&self) -> bool {
        self.expert_count.is_some_and(|n| n > 0)
    }
}

/// The name of a `general.file_type` (llama.cpp's `llama_ftype`), or `None` for a number that is not one of the known ones. From llama.cpp's header as it stood when this was
/// written; a newer number is simply not named, and nothing else depends on the name.
pub fn quant_name(file_type: u32) -> Option<&'static str> {
    Some(match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        _ => return None,
    })
}

/// A reader that counts what it has gone through, so that no header takes more than [`MAX_HEADER_BYTES`] however it is made.
struct Reader<R> {
    inner: R,
    used: u64,
}

impl<R: Read> Reader<R> {
    fn new(inner: R) -> Self {
        Self { inner, used: 0 }
    }

    fn spend(&mut self, n: u64) -> Result<(), GgufError> {
        self.used = self.used.saturating_add(n);
        if self.used > MAX_HEADER_BYTES {
            return Err(GgufError::TooLarge);
        }
        Ok(())
    }

    fn fill(&mut self, buf: &mut [u8], what: &'static str) -> Result<(), GgufError> {
        self.spend(buf.len() as u64)?;
        self.inner.read_exact(buf).map_err(|e| if e.kind() == std::io::ErrorKind::UnexpectedEof { GgufError::Truncated(what) } else { GgufError::Unreadable(e.to_string()) })
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, GgufError> {
        let mut b = [0u8; 1];
        self.fill(&mut b, what)?;
        Ok(b[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, GgufError> {
        let mut b = [0u8; 2];
        self.fill(&mut b, what)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, GgufError> {
        let mut b = [0u8; 4];
        self.fill(&mut b, what)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, GgufError> {
        let mut b = [0u8; 8];
        self.fill(&mut b, what)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Reads and throws away `n` bytes: nothing is allocated for them, however large `n` is.
    fn skip(&mut self, n: u64, what: &'static str) -> Result<(), GgufError> {
        self.spend(n)?;
        let copied = std::io::copy(&mut (&mut self.inner).take(n), &mut std::io::sink()).map_err(|e| GgufError::Unreadable(e.to_string()))?;
        if copied < n {
            return Err(GgufError::Truncated(what));
        }
        Ok(())
    }

    /// The length of a string, held to `max`.
    fn string_len(&mut self, what: &'static str, max: u64) -> Result<u64, GgufError> {
        let len = self.u64(what)?;
        if len > max {
            return Err(GgufError::StringTooLong { what, len });
        }
        Ok(len)
    }

    /// A string of at most `max` bytes (damaged UTF-8 is replaced, not refused: a name is only shown).
    fn string(&mut self, what: &'static str, max: u64) -> Result<String, GgufError> {
        let len = self.string_len(what, max)?;
        let mut bytes = vec![0u8; len as usize];
        self.fill(&mut bytes, what)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn skip_string(&mut self, what: &'static str) -> Result<u64, GgufError> {
        let len = self.string_len(what, MAX_STRING)?;
        self.skip(len, what)?;
        Ok(len)
    }
}

/// The size in bytes of a value type that has a fixed size, and `None` for a string (8) or an array (9) or a type that does not exist.
fn fixed_size(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// A number from a value of a numeric type (0 to 5, 10, 11 are integers, 7 a flag), or `None` for a float, a string or an array.
fn read_integer<R: Read>(r: &mut Reader<R>, ty: u32, what: &'static str) -> Result<Option<u64>, GgufError> {
    Ok(match ty {
        0 | 1 | 7 => Some(u64::from(r.u8(what)?)),
        2 | 3 => Some(u64::from(r.u16(what)?)),
        4 | 5 => Some(u64::from(r.u32(what)?)),
        10 | 11 => Some(r.u64(what)?),
        6 => {
            r.skip(4, what)?;
            None
        }
        12 => {
            r.skip(8, what)?;
            None
        }
        _ => None,
    })
}

/// Goes over a value and keeps nothing of it. Returns how many elements it was when it is an array.
fn skip_value<R: Read>(r: &mut Reader<R>, ty: u32, depth: u32) -> Result<u64, GgufError> {
    match ty {
        8 => {
            r.skip_string("a text value")?;
            Ok(1)
        }
        9 => {
            if depth >= MAX_DEPTH {
                return Err(GgufError::TooDeep);
            }
            let element = r.u32("a list")?;
            let len = r.u64("a list")?;
            if len > MAX_ARRAY {
                return Err(GgufError::ArrayTooLong(len));
            }
            skip_elements(r, element, len, depth + 1)?;
            Ok(len)
        }
        _ => match fixed_size(ty) {
            Some(size) => {
                r.skip(size, "a value")?;
                Ok(1)
            }
            None => Err(GgufError::ValueType(ty)),
        },
    }
}

fn skip_elements<R: Read>(r: &mut Reader<R>, element: u32, len: u64, depth: u32) -> Result<(), GgufError> {
    if let Some(size) = fixed_size(element) {
        // `len` is at most MAX_ARRAY and `size` at most 8, so this cannot overflow; the header budget decides whether it is too much.
        return r.skip(len * size, "a list");
    }
    match element {
        8 => {
            for _ in 0..len {
                r.skip_string("a list of texts")?;
            }
            Ok(())
        }
        9 => {
            for _ in 0..len {
                skip_value(r, 9, depth)?;
            }
            Ok(())
        }
        other => Err(GgufError::ValueType(other)),
    }
}

/// The largest element of a numeric array (the heads of each layer, for a model whose layers differ), reading at most [`MAX_READ_ARRAY`] of them; longer ones are skipped and give `None`.
fn array_max<R: Read>(r: &mut Reader<R>, element: u32, len: u64) -> Result<Option<u64>, GgufError> {
    if len == 0 || len > MAX_READ_ARRAY || !matches!(element, 0..=5 | 10 | 11) {
        skip_elements(r, element, len, MAX_DEPTH)?;
        return Ok(None);
    }
    let mut most = 0u64;
    for _ in 0..len {
        most = most.max(read_integer(r, element, "a list of numbers")?.unwrap_or(0));
    }
    Ok(Some(most))
}

/// What a pair is kept for, by the end of its key (the start is the architecture, which is not known until `general.architecture` has been read).
const ARCH_SUFFIXES: [&str; 9] = [".context_length", ".block_count", ".embedding_length", ".attention.head_count", ".attention.head_count_kv", ".attention.key_length", ".attention.value_length", ".expert_count", ".expert_used_count"];

/// The numbers that belong to an architecture's own keys, held until the architecture is known. At most as many entries as there are suffixes times a few prefixes.
const MAX_KEPT: usize = 64;

/// The little-endian number of eight bytes that starts at `at` in `bytes`. A start that has fewer than eight bytes after it is a file that is too short to be a GGUF file.
fn u64_at(bytes: &[u8], at: usize) -> Result<u64, GgufError> {
    let end = at.checked_add(8).ok_or(GgufError::TooShort)?;
    let eight: [u8; 8] = bytes.get(at..end).and_then(|s| s.try_into().ok()).ok_or(GgufError::TooShort)?;
    Ok(u64::from_le_bytes(eight))
}

/// Reads a GGUF header from `source` (a file, or bytes in memory). `file_bytes` is the length of the file when it is known (0 otherwise), only for the size of the result.
pub fn read_header<R: Read>(source: R, file_bytes: u64) -> Result<GgufInfo, GgufError> {
    let mut r = Reader::new(source);
    let mut marker = [0u8; 4];
    // Fewer than four bytes is a file that is too short; the marker itself is checked as soon as it is there.
    if r.inner.read_exact(&mut marker).is_err() {
        return Err(GgufError::TooShort);
    }
    r.used = 4;
    if &marker != b"GGUF" {
        return Err(GgufError::NotGguf);
    }
    let mut head = [0u8; 20];
    if r.inner.read_exact(&mut head).is_err() {
        return Err(GgufError::TooShort);
    }
    r.used += 20;
    let version = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    if !(2..=3).contains(&version) {
        // A big-endian file has its version's low byte last: 3 reads as 0x0300_0000.
        if version.swap_bytes() == 2 || version.swap_bytes() == 3 {
            return Err(GgufError::BigEndian);
        }
        return Err(GgufError::Version(version));
    }
    let tensor_count = u64_at(&head, 4)?;
    let pair_count = u64_at(&head, 12)?;
    if tensor_count > MAX_TENSORS || pair_count > MAX_KV {
        return Err(GgufError::Counts { tensors: tensor_count, pairs: pair_count });
    }

    let mut info = GgufInfo {
        version,
        tensor_count,
        pair_count,
        file_bytes,
        architecture: String::new(),
        name: None,
        size_label: None,
        file_type: None,
        quant: None,
        context_length: None,
        block_count: None,
        embedding_length: None,
        head_count: None,
        head_count_kv: None,
        key_length: None,
        value_length: None,
        expert_count: None,
        vocab_size: None,
        has_chat_template: false,
        split: None,
    };
    let mut kept: BTreeMap<String, u64> = BTreeMap::new();
    let (mut split_no, mut split_count) = (None, None);
    for _ in 0..pair_count {
        let key_len = r.string_len("a setting's name", MAX_KEY)?;
        let mut key_bytes = vec![0u8; key_len as usize];
        r.fill(&mut key_bytes, "a setting's name")?;
        let key = String::from_utf8_lossy(&key_bytes).into_owned();
        let ty = r.u32("a setting")?;
        match (key.as_str(), ty) {
            ("general.architecture", 8) => info.architecture = r.string("the architecture", 256)?.to_ascii_lowercase(),
            ("general.name", 8) => info.name = Some(clip(r.string("the model name", MAX_STRING)?, 200)),
            ("general.size_label", 8) => info.size_label = Some(clip(r.string("the size label", MAX_STRING)?, 40)),
            ("tokenizer.chat_template", 8) => {
                let len = r.skip_string("the chat template")?;
                info.has_chat_template = len > 0;
            }
            ("tokenizer.ggml.tokens", 9) => {
                let element = r.u32("the vocabulary")?;
                let len = r.u64("the vocabulary")?;
                if len > MAX_ARRAY {
                    return Err(GgufError::ArrayTooLong(len));
                }
                skip_elements(&mut r, element, len, 1)?;
                info.vocab_size = Some(len);
            }
            ("general.file_type", _) if fixed_size(ty).is_some() => {
                if let Some(n) = read_integer(&mut r, ty, "the file type")? {
                    info.file_type = u32::try_from(n).ok();
                }
            }
            ("split.no", _) if fixed_size(ty).is_some() => split_no = read_integer(&mut r, ty, "the split number")?,
            ("split.count", _) if fixed_size(ty).is_some() => split_count = read_integer(&mut r, ty, "the split count")?,
            (k, _) if ARCH_SUFFIXES.iter().any(|s| k.ends_with(s)) && kept.len() < MAX_KEPT => {
                if ty == 9 {
                    let element = r.u32("a list of numbers")?;
                    let len = r.u64("a list of numbers")?;
                    if len > MAX_ARRAY {
                        return Err(GgufError::ArrayTooLong(len));
                    }
                    if let Some(most) = array_max(&mut r, element, len)? {
                        kept.insert(key.clone(), most);
                    }
                } else if fixed_size(ty).is_some() {
                    if let Some(n) = read_integer(&mut r, ty, "a number")? {
                        kept.insert(key.clone(), n);
                    }
                } else {
                    skip_value(&mut r, ty, 0)?;
                }
            }
            _ => {
                skip_value(&mut r, ty, 0)?;
            }
        }
    }

    let own = |suffix: &str| -> Option<u64> { kept.get(&format!("{}{suffix}", info.architecture)).copied() };
    let small = |n: Option<u64>| n.and_then(|v| u32::try_from(v).ok());
    info.context_length = own(".context_length");
    info.block_count = small(own(".block_count"));
    info.embedding_length = small(own(".embedding_length"));
    info.head_count = small(own(".attention.head_count"));
    info.head_count_kv = small(own(".attention.head_count_kv"));
    info.key_length = small(own(".attention.key_length"));
    info.value_length = small(own(".attention.value_length"));
    info.expert_count = small(own(".expert_count"));
    info.quant = info.file_type.and_then(quant_name).map(str::to_string);
    info.split = match (split_no, split_count) {
        (Some(no), Some(count)) if count > 1 => u32::try_from(no).ok().zip(u32::try_from(count).ok()),
        _ => None,
    };
    Ok(info)
}

fn clip(mut text: String, max_chars: usize) -> String {
    if let Some((cut, _)) = text.char_indices().nth(max_chars) {
        text.truncate(cut);
    }
    text
}

/// Reads the header of the file at `path`. A file shorter than the start of a header is [`GgufError::TooShort`] before anything is read.
pub fn read_path(path: &Path) -> Result<GgufInfo, GgufError> {
    let file = std::fs::File::open(path).map_err(|e| GgufError::Unreadable(e.kind().to_string()))?;
    let bytes = file.metadata().map(|m| m.len()).map_err(|e| GgufError::Unreadable(e.kind().to_string()))?;
    if bytes < FIXED_HEAD_BYTES {
        return Err(GgufError::TooShort);
    }
    read_header(std::io::BufReader::with_capacity(256 * 1024, file), bytes)
}

type Cached = (u64, Option<SystemTime>, Result<GgufInfo, GgufError>);

static CACHE: Mutex<Vec<(PathBuf, Cached)>> = Mutex::new(Vec::new());
const CACHE_SIZE: usize = 64;

/// [`read_path`], remembered: a listing of the models is asked for again and again (every time the screen refreshes) and a header is read again only when the file's length or its
/// time of change is not what it was. The newest [`CACHE_SIZE`] files are kept.
pub fn read_cached(path: &Path) -> Result<GgufInfo, GgufError> {
    let (len, modified) = std::fs::metadata(path).map(|m| (m.len(), m.modified().ok())).map_err(|e| GgufError::Unreadable(e.kind().to_string()))?;
    {
        let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, (l, m, result))) = cache.iter().find(|(p, _)| p == path) {
            if *l == len && *m == modified {
                return result.clone();
            }
        }
    }
    let result = read_path(path);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|(p, _)| p != path);
    if cache.len() >= CACHE_SIZE {
        let _oldest = cache.remove(0);
    }
    cache.push((path.to_path_buf(), (len, modified, result.clone())));
    result
}

/// The family an architecture name belongs to, for the architectures Aqua knows how to talk to (`None` for any other: the file name decides then).
pub fn family_of_architecture(architecture: &str) -> Option<super::Family> {
    use super::Family;
    match architecture.trim().to_ascii_lowercase().as_str() {
        "qwen3" | "qwen3moe" => Some(Family::Qwen3),
        "gemma3" | "gemma3n" => Some(Family::Gemma3),
        "phi3" | "phi4" | "phimoe" => Some(Family::Phi),
        "granite" | "granitemoe" | "granitehybrid" => Some(Family::Granite),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds the bytes of a header: `pairs` is the count to write, the body is whatever the test puts after it.
    fn header(version: u32, tensors: u64, pairs: u64, body: &[u8]) -> Vec<u8> {
        let mut v = b"GGUF".to_vec();
        v.extend(version.to_le_bytes());
        v.extend(tensors.to_le_bytes());
        v.extend(pairs.to_le_bytes());
        v.extend(body);
        v
    }

    fn string(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u64).to_le_bytes().to_vec();
        v.extend(s.as_bytes());
        v
    }

    fn pair_string(key: &str, value: &str) -> Vec<u8> {
        let mut v = string(key);
        v.extend(8u32.to_le_bytes());
        v.extend(string(value));
        v
    }

    fn pair_u32(key: &str, value: u32) -> Vec<u8> {
        let mut v = string(key);
        v.extend(4u32.to_le_bytes());
        v.extend(value.to_le_bytes());
        v
    }

    fn pair_u64(key: &str, value: u64) -> Vec<u8> {
        let mut v = string(key);
        v.extend(10u32.to_le_bytes());
        v.extend(value.to_le_bytes());
        v
    }

    /// An array of texts: the vocabulary of a toy model.
    fn pair_texts(key: &str, items: &[&str]) -> Vec<u8> {
        let mut v = string(key);
        v.extend(9u32.to_le_bytes());
        v.extend(8u32.to_le_bytes());
        v.extend((items.len() as u64).to_le_bytes());
        for i in items {
            v.extend(string(i));
        }
        v
    }

    fn parse(bytes: &[u8]) -> Result<GgufInfo, GgufError> {
        read_header(Cursor::new(bytes), bytes.len() as u64)
    }

    /// The header of a toy Qwen3-8B: the numbers of the real model (36 layers, 4096 wide, 32 heads of which 8 for the cache, 40,960 tokens of trained context, Q4_K_M).
    fn qwen3_8b() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(pair_string("general.architecture", "qwen3"));
        body.extend(pair_string("general.name", "Qwen3 8B"));
        body.extend(pair_string("general.size_label", "8B"));
        body.extend(pair_u32("general.file_type", 15));
        body.extend(pair_u32("qwen3.block_count", 36));
        body.extend(pair_u32("qwen3.context_length", 40960));
        body.extend(pair_u32("qwen3.embedding_length", 4096));
        body.extend(pair_u32("qwen3.attention.head_count", 32));
        body.extend(pair_u32("qwen3.attention.head_count_kv", 8));
        body.extend(pair_u32("qwen3.attention.key_length", 128));
        body.extend(pair_u32("qwen3.attention.value_length", 128));
        body.extend(pair_texts("tokenizer.ggml.tokens", &["a", "b", "c"]));
        body.extend(pair_texts("tokenizer.ggml.merges", &["a b", "b c"]));
        body.extend(pair_string("tokenizer.chat_template", "{% for m in messages %}{{ m.content }}{% endfor %}"));
        header(3, 399, 14, &body)
    }

    #[test]
    fn a_header_of_the_start_of_a_file_as_bytes_is_read() {
        // 24 bytes of start and one setting, written out by hand: GGUF, version 3, 0 tensors, 1 setting, the key "general.architecture" and the text "llama".
        let bytes: &[u8] = &[
            b'G', b'G', b'U', b'F', 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, //
            20, 0, 0, 0, 0, 0, 0, 0, b'g', b'e', b'n', b'e', b'r', b'a', b'l', b'.', b'a', b'r', b'c', b'h', b'i', b't', b'e', b'c', b't', b'u', b'r', b'e', //
            8, 0, 0, 0, //
            5, 0, 0, 0, 0, 0, 0, 0, b'l', b'l', b'a', b'm', b'a',
        ];
        let info = parse(bytes).expect("a sound header");
        assert_eq!((info.version, info.tensor_count, info.pair_count), (3, 0, 1));
        assert_eq!(info.architecture, "llama");
        assert_eq!(info.block_count, None, "what the header does not say is not made up");
    }

    #[test]
    fn the_numbers_of_a_qwen3_8b_header_come_out_and_its_cache_is_what_the_arithmetic_says() {
        let info = parse(&qwen3_8b()).expect("a sound header");
        assert_eq!(info.architecture, "qwen3");
        assert_eq!((info.name.as_deref(), info.size_label.as_deref()), (Some("Qwen3 8B"), Some("8B")));
        assert_eq!((info.file_type, info.quant.as_deref()), (Some(15), Some("Q4_K_M")));
        assert_eq!((info.block_count, info.context_length, info.embedding_length), (Some(36), Some(40960), Some(4096)));
        assert_eq!((info.head_count, info.head_count_kv, info.key_length, info.value_length), (Some(32), Some(8), Some(128), Some(128)));
        assert_eq!(info.vocab_size, Some(3), "the vocabulary is counted and not kept");
        assert!(info.has_chat_template);
        assert_eq!(info.split, None);
        assert!(!info.is_moe());
        // 8 heads x (128 + 128) = 2048 elements a token a layer; at a window of 6144 tokens, 36 layers and 2 bytes: 36 x 6144 x 2048 x 2.
        assert_eq!(info.kv_elements_per_token_per_layer(), Some(2048));
        assert_eq!(info.kv_bytes(36, 6144, 2), Some(905_969_664));
        assert_eq!(info.kv_bytes(22, 6144, 2), Some(553_648_128), "the layers on the card hold their own share of it");
    }

    #[test]
    fn the_width_of_a_layer_is_worked_out_when_the_lengths_of_keys_and_values_are_not_given() {
        let mut body = pair_string("general.architecture", "llama");
        body.extend(pair_u32("llama.block_count", 32));
        body.extend(pair_u32("llama.embedding_length", 4096));
        body.extend(pair_u32("llama.attention.head_count", 32));
        body.extend(pair_u32("llama.attention.head_count_kv", 8));
        let info = parse(&header(3, 291, 5, &body)).unwrap();
        assert_eq!(info.kv_elements_per_token_per_layer(), Some(8 * (128 + 128)));
        // No cache heads given: the cache has as many as the attention has.
        let mut body = pair_string("general.architecture", "llama");
        body.extend(pair_u32("llama.embedding_length", 2048));
        body.extend(pair_u32("llama.attention.head_count", 16));
        let info = parse(&header(3, 0, 3, &body)).unwrap();
        assert_eq!(info.kv_elements_per_token_per_layer(), Some(16 * (128 + 128)));
        // Nothing to go on: no number.
        assert_eq!(parse(&header(3, 0, 0, &[])).unwrap().kv_bytes(10, 100, 2), None);
    }

    #[test]
    fn the_weight_of_a_layer_is_the_file_over_the_layers_plus_one_and_only_when_both_are_known() {
        let mut info = parse(&qwen3_8b()).unwrap();
        assert_eq!(info.bytes_per_layer(), Some(info.file_bytes / 37));
        info.file_bytes = 5_027_783_488;
        assert_eq!(info.bytes_per_layer(), Some(135_886_040));
        info.file_bytes = 0;
        assert_eq!(info.bytes_per_layer(), None, "a header read from memory has no file size");
        let none = parse(&header(3, 0, 0, &[])).unwrap();
        assert_eq!(none.bytes_per_layer(), None);
    }

    #[test]
    fn settings_in_any_order_and_a_architecture_that_comes_last_are_read_the_same() {
        // The architecture after the numbers that belong to it (the format does not fix the order), and another architecture's numbers in the file are not taken.
        let mut body = pair_u32("qwen3.block_count", 36);
        body.extend(pair_u32("llama.block_count", 99));
        body.extend(pair_string("general.architecture", "qwen3"));
        let info = parse(&header(3, 0, 3, &body)).unwrap();
        assert_eq!(info.block_count, Some(36));
    }

    #[test]
    fn version_2_is_read_and_versions_1_and_4_and_a_big_endian_file_are_named() {
        assert!(parse(&header(2, 0, 0, &[])).is_ok());
        assert_eq!(parse(&header(1, 0, 0, &[])), Err(GgufError::Version(1)));
        assert_eq!(parse(&header(4, 0, 0, &[])), Err(GgufError::Version(4)));
        let mut big = b"GGUF".to_vec();
        big.extend(3u32.to_be_bytes());
        big.extend([0u8; 16]);
        assert_eq!(parse(&big), Err(GgufError::BigEndian));
        assert!(GgufError::BigEndian.to_string().contains("big-endian"));
        assert!(GgufError::Version(1).to_string().contains("version 1"));
    }

    #[test]
    fn a_file_that_is_not_gguf_or_is_too_short_is_refused_with_a_plain_reason() {
        assert_eq!(parse(b"This is a text file that was renamed to .gguf, as a joke.\n"), Err(GgufError::NotGguf));
        assert_eq!(parse(b"GGUF"), Err(GgufError::TooShort));
        assert_eq!(parse(b""), Err(GgufError::TooShort));
        assert_eq!(parse(b"GG"), Err(GgufError::TooShort));
        for e in [GgufError::NotGguf, GgufError::TooShort, GgufError::Truncated("a value"), GgufError::TooLarge] {
            let said = e.to_string();
            assert!(!said.contains('\\') && !said.contains(":/"), "no path in {said}");
            assert!(said.starts_with("the "), "{said}");
        }
    }

    #[test]
    fn a_header_cut_short_in_its_counts_is_too_short_and_never_a_panic() {
        let whole = header(3, 399, 14, &[]);
        // After the marker and the version (8 bytes), and after the first four bytes of the number of tensors (12 bytes).
        assert_eq!(parse(&whole[..8]), Err(GgufError::TooShort));
        assert_eq!(parse(&whole[..12]), Err(GgufError::TooShort));
        // At every length short of the fixed start of 24 bytes.
        for cut in 0..whole.len() {
            assert_eq!(parse(&whole[..cut]), Err(GgufError::TooShort), "cut at {cut}");
        }
        // With the whole of it, the counts are read, and what follows is what is missing.
        assert!(matches!(parse(&whole), Err(GgufError::Truncated(_))));
    }

    #[test]
    fn eight_bytes_are_read_at_a_place_and_a_header_that_ends_before_them_is_too_short() {
        let bytes: Vec<u8> = (1..=20).collect();
        assert_eq!(u64_at(&bytes, 4), Ok(u64::from_le_bytes([5, 6, 7, 8, 9, 10, 11, 12])));
        assert_eq!(u64_at(&bytes, 12), Ok(u64::from_le_bytes([13, 14, 15, 16, 17, 18, 19, 20])));
        assert_eq!(u64_at(&bytes, 13), Err(GgufError::TooShort));
        assert_eq!(u64_at(&bytes[..11], 4), Err(GgufError::TooShort));
        assert_eq!(u64_at(&bytes[..12], 4), Ok(u64::from_le_bytes([5, 6, 7, 8, 9, 10, 11, 12])));
        assert_eq!(u64_at(&[], 0), Err(GgufError::TooShort));
        assert_eq!(u64_at(&bytes, usize::MAX), Err(GgufError::TooShort));
    }

    #[test]
    fn counts_beyond_the_ceilings_are_refused_before_anything_is_read_for_them() {
        assert_eq!(parse(&header(3, 0, MAX_KV + 1, &[])), Err(GgufError::Counts { tensors: 0, pairs: MAX_KV + 1 }));
        assert_eq!(parse(&header(3, MAX_TENSORS + 1, 0, &[])), Err(GgufError::Counts { tensors: MAX_TENSORS + 1, pairs: 0 }));
        assert_eq!(parse(&header(3, u64::MAX, u64::MAX, &[])), Err(GgufError::Counts { tensors: u64::MAX, pairs: u64::MAX }));
        // A count at the ceiling with no settings behind it is a truncated file, not a crash and not a long wait.
        assert!(matches!(parse(&header(3, 0, MAX_KV, &[])), Err(GgufError::Truncated(_))));
    }

    #[test]
    fn a_string_that_claims_more_than_a_megabyte_is_refused_without_being_allocated() {
        // The key claims 2^40 bytes and the file ends there: the length is what is refused.
        let mut body = (1u64 << 40).to_le_bytes().to_vec();
        body.extend(b"general.architecture");
        assert!(matches!(parse(&header(3, 0, 1, &body)), Err(GgufError::StringTooLong { what: "a setting's name", .. })));
        // A value that claims one byte more than the ceiling.
        let mut body = string("general.name");
        body.extend(8u32.to_le_bytes());
        body.extend((MAX_STRING + 1).to_le_bytes());
        assert!(matches!(parse(&header(3, 0, 1, &body)), Err(GgufError::StringTooLong { .. })));
        // The same for a text that is only skipped (the chat template).
        let mut body = string("tokenizer.chat_template");
        body.extend(8u32.to_le_bytes());
        body.extend((MAX_STRING + 1).to_le_bytes());
        assert!(matches!(parse(&header(3, 0, 1, &body)), Err(GgufError::StringTooLong { .. })));
    }

    #[test]
    fn a_header_that_ends_in_the_middle_of_a_value_says_where() {
        let whole = qwen3_8b();
        for cut in [30, 60, 100, whole.len() - 3] {
            let result = parse(&whole[..cut]);
            assert!(matches!(result, Err(GgufError::Truncated(_))), "cut at {cut}: {result:?}");
        }
        let said = parse(&whole[..60]).unwrap_err().to_string();
        assert!(said.contains("damaged or not finished downloading"), "{said}");
    }

    #[test]
    fn lists_are_skipped_by_their_size_and_a_list_that_claims_too_much_is_refused() {
        // A list of 1,000 bytes is skipped whole; the setting after it is read.
        let mut body = string("tokenizer.ggml.token_type");
        body.extend(9u32.to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(1000u64.to_le_bytes());
        body.extend(vec![0u8; 1000]);
        body.extend(pair_string("general.architecture", "llama"));
        assert_eq!(parse(&header(3, 0, 2, &body)).unwrap().architecture, "llama");
        // More than the ceiling of entries.
        let mut body = string("tokenizer.ggml.tokens");
        body.extend(9u32.to_le_bytes());
        body.extend(8u32.to_le_bytes());
        body.extend((MAX_ARRAY + 1).to_le_bytes());
        assert_eq!(parse(&header(3, 0, 1, &body)), Err(GgufError::ArrayTooLong(MAX_ARRAY + 1)));
        // A list of eight-byte numbers whose size is more than the header may be: refused as too large before a byte of it is read.
        let mut body = string("tokenizer.ggml.scores");
        body.extend(9u32.to_le_bytes());
        body.extend(12u32.to_le_bytes());
        body.extend(MAX_ARRAY.to_le_bytes());
        let result = parse(&header(3, 0, 1, &body));
        assert!(matches!(result, Err(GgufError::TooLarge | GgufError::Truncated(_))), "{result:?}");
        // Lists of the largest size (128 million bytes each), really there: two fit in the budget of a header (256 MiB) and the third is refused before a byte of it is read (the bytes
        // come from a source that never ends, so that a reader that trusted a length would never come back).
        let list = |extra: &[u8]| {
            let mut v = extra.to_vec();
            v.extend(string("tokenizer.ggml.scores"));
            v.extend(9u32.to_le_bytes());
            v.extend(12u32.to_le_bytes());
            v.extend(MAX_ARRAY.to_le_bytes());
            v
        };
        let endless = Cursor::new(list(&header(3, 0, 3, &[])))
            .chain(std::io::repeat(0).take(MAX_ARRAY * 8))
            .chain(Cursor::new(list(&[])))
            .chain(std::io::repeat(0).take(MAX_ARRAY * 8))
            .chain(Cursor::new(list(&[])))
            .chain(std::io::repeat(0));
        assert_eq!(read_header(endless, 0), Err(GgufError::TooLarge), "a header that never ends is stopped by its budget");
        // A list that claims entries the file does not have.
        let mut body = string("tokenizer.ggml.scores");
        body.extend(9u32.to_le_bytes());
        body.extend(6u32.to_le_bytes());
        body.extend(5_000_000u64.to_le_bytes());
        body.extend([0u8; 16]);
        assert!(matches!(parse(&header(3, 0, 1, &body)), Err(GgufError::Truncated("a list"))));
    }

    #[test]
    fn lists_inside_lists_are_followed_to_a_depth_and_no_deeper() {
        // [[1, 2], [3]] as an array of arrays of bytes.
        let mut body = string("odd.nested");
        body.extend(9u32.to_le_bytes());
        body.extend(9u32.to_le_bytes());
        body.extend(2u64.to_le_bytes());
        for items in [&[1u8, 2][..], &[3][..]] {
            body.extend(0u32.to_le_bytes());
            body.extend((items.len() as u64).to_le_bytes());
            body.extend(items);
        }
        body.extend(pair_string("general.architecture", "llama"));
        assert_eq!(parse(&header(3, 0, 2, &body)).unwrap().architecture, "llama");
        // Arrays inside arrays inside arrays, past the depth: refused, not followed into the stack.
        let mut deep = Vec::new();
        for _ in 0..(MAX_DEPTH + 2) {
            deep.extend(9u32.to_le_bytes());
            deep.extend(1u64.to_le_bytes());
        }
        let mut body = string("odd.deep");
        body.extend(9u32.to_le_bytes());
        body.extend(deep);
        assert_eq!(parse(&header(3, 0, 1, &body)), Err(GgufError::TooDeep));
    }

    #[test]
    fn a_value_of_a_type_the_format_does_not_have_is_refused() {
        let mut body = string("general.name");
        body.extend(13u32.to_le_bytes());
        body.extend([0u8; 8]);
        assert_eq!(parse(&header(3, 0, 1, &body)), Err(GgufError::ValueType(13)));
    }

    #[test]
    fn heads_given_for_each_layer_are_taken_as_the_most_of_them_and_a_long_list_is_left_alone() {
        let mut body = pair_string("general.architecture", "granite");
        // head_count_kv as an array of 4 numbers.
        body.extend(string("granite.attention.head_count_kv"));
        body.extend(9u32.to_le_bytes());
        body.extend(4u32.to_le_bytes());
        body.extend(4u64.to_le_bytes());
        for n in [0u32, 8, 0, 4] {
            body.extend(n.to_le_bytes());
        }
        body.extend(pair_u32("granite.block_count", 4));
        let info = parse(&header(3, 0, 3, &body)).unwrap();
        assert_eq!((info.head_count_kv, info.block_count), (Some(8), Some(4)));
        // More entries than are worth reading: skipped, and the number is simply not there.
        let mut body = pair_string("general.architecture", "granite");
        body.extend(string("granite.attention.head_count"));
        body.extend(9u32.to_le_bytes());
        body.extend(4u32.to_le_bytes());
        body.extend((MAX_READ_ARRAY + 1).to_le_bytes());
        body.extend(vec![0u8; (MAX_READ_ARRAY as usize + 1) * 4]);
        let info = parse(&header(3, 0, 2, &body)).unwrap();
        assert_eq!(info.head_count, None);
    }

    #[test]
    fn an_experts_count_marks_a_mixture_and_a_split_file_says_which_part_it_is() {
        let mut body = pair_string("general.architecture", "qwen3moe");
        body.extend(pair_u32("qwen3moe.expert_count", 128));
        body.extend(pair_u32("qwen3moe.block_count", 48));
        // Written as 16-bit and 64-bit numbers too: the format lets a file use any size.
        body.extend(pair_u64("split.count", 3));
        body.extend({
            let mut v = string("split.no");
            v.extend(2u32.to_le_bytes());
            v.extend(1u16.to_le_bytes());
            v
        });
        let info = parse(&header(3, 0, 5, &body)).unwrap();
        assert!(info.is_moe());
        assert_eq!(info.expert_count, Some(128));
        assert_eq!(info.split, Some((1, 3)));
        // One part of one is not split.
        let mut body = pair_u32("split.no", 0);
        body.extend(pair_u32("split.count", 1));
        assert_eq!(parse(&header(3, 0, 2, &body)).unwrap().split, None);
    }

    #[test]
    fn the_known_quantisations_are_named_and_a_new_one_is_not_made_up() {
        assert_eq!(quant_name(15), Some("Q4_K_M"));
        assert_eq!(quant_name(7), Some("Q8_0"));
        assert_eq!(quant_name(1), Some("F16"));
        assert_eq!(quant_name(32), Some("BF16"));
        assert_eq!(quant_name(4), None, "4 and 5 were removed from llama.cpp");
        assert_eq!(quant_name(9999), None);
    }

    #[test]
    fn family_comes_from_the_architecture_for_the_ones_known_and_not_for_the_rest() {
        use super::super::Family;
        assert_eq!(family_of_architecture("qwen3"), Some(Family::Qwen3));
        assert_eq!(family_of_architecture("QWEN3MOE"), Some(Family::Qwen3));
        assert_eq!(family_of_architecture("gemma3"), Some(Family::Gemma3));
        assert_eq!(family_of_architecture("llama"), None, "a llama architecture is also Mistral and others: the name decides");
        assert_eq!(family_of_architecture(""), None);
    }

    #[test]
    fn a_name_longer_than_it_should_be_is_cut_and_damaged_text_does_not_fail_the_read() {
        let long = "x".repeat(5000);
        let info = parse(&header(3, 0, 1, &pair_string("general.name", &long))).unwrap();
        assert_eq!(info.name.as_deref().map(str::len), Some(200));
        let mut body = string("general.name");
        body.extend(8u32.to_le_bytes());
        body.extend(3u64.to_le_bytes());
        body.extend([0xff, 0xfe, b'a']);
        let info = parse(&header(3, 0, 1, &body)).unwrap();
        assert!(info.name.unwrap().ends_with('a'));
    }

    #[test]
    fn a_file_is_read_by_path_and_a_header_that_changed_is_read_again() {
        use crate::test_support::TempWorld;
        let world = TempWorld::new("gguf_path");
        let path = world.write("m.gguf", qwen3_8b());
        let info = read_path(&path).unwrap();
        assert_eq!(info.file_bytes, qwen3_8b().len() as u64);
        assert_eq!(info.block_count, Some(36));
        // Cached: the same answer while the file is the same.
        assert_eq!(read_cached(&path).unwrap(), info);
        assert_eq!(read_cached(&path).unwrap(), info);
        // Changed (a different length): read again.
        let mut other = Vec::new();
        other.extend(pair_string("general.architecture", "llama"));
        other.extend(pair_u32("llama.block_count", 28));
        std::fs::write(&path, header(3, 0, 2, &other)).unwrap();
        assert_eq!(read_cached(&path).unwrap().block_count, Some(28));
        // A file of the wrong kind and one too short, by path.
        let text = world.write("t.gguf", "not a model at all, only some text that is long enough");
        assert_eq!(read_path(&text), Err(GgufError::NotGguf));
        let tiny = world.write("tiny.gguf", "GGUF");
        assert_eq!(read_path(&tiny), Err(GgufError::TooShort));
        assert!(matches!(read_path(&world.path("missing.gguf")), Err(GgufError::Unreadable(_))));
        // The failure is remembered too, and does not hold on after the file is mended.
        assert_eq!(read_cached(&text), Err(GgufError::NotGguf));
        std::fs::write(&text, header(3, 0, 1, &pair_string("general.architecture", "qwen3"))).unwrap();
        assert_eq!(read_cached(&text).unwrap().architecture, "qwen3");
    }

    /// Whatever bytes it is given, the reader returns: it does not panic, loop or allocate by what a length says. (Not a fuzzer; a few thousand shapes of damaged header.)
    #[test]
    fn damaged_headers_of_every_cut_and_every_flipped_byte_come_back_and_never_panic() {
        let whole = qwen3_8b();
        for cut in 0..whole.len() {
            let _ = parse(&whole[..cut]);
        }
        for at in 0..whole.len().min(400) {
            let mut bytes = whole.clone();
            bytes[at] ^= 0xff;
            let _ = parse(&bytes);
            bytes[at] = 0x7f;
            let _ = parse(&bytes);
        }
        // A pseudo-random body behind a sound start.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..300 {
            let mut body = Vec::new();
            for _ in 0..(round % 200 + 8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                body.push(seed as u8);
            }
            let _ = parse(&header(3, 5, 3, &body));
        }
    }
}
