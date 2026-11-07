//! The embedding models RustCodeLore can run: bge-m3 (the default where a GPU
//! works), multilingual-e5-base (the CPU fallback and the default there), and
//! nomic-embed-text-v1.5 (kept for the parity tests only).
//!
//! Each model names its Hugging Face repository, the exact revision, the files
//! with their pinned sizes and SHA-256s, its dimension, its pooling and its
//! task prefixes. `lore.toml`'s `embed.model` selects one (or "auto").
use candle_core::Device;

/// One file of a model, pinned by its exact size and SHA-256.
#[derive(Clone, Copy, Debug)]
pub struct PinnedFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// How the token vectors become the record's vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pooling {
    /// The [CLS] token, as bge-m3 is trained.
    Cls,
    /// The attention-mask mean, as multilingual-e5-base is trained.
    Mean,
}

/// One supported embedding model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddingModel {
    BgeM3,
    E5Base,
    Nomic,
}

impl EmbeddingModel {
    pub const ALL: [Self; 3] = [Self::BgeM3, Self::E5Base, Self::Nomic];

    /// The name in `lore.toml` (`embed.model`), with the aliases a person may
    /// write. "auto" is the caller's job (see [`default_for`]).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "bge-m3" | "bge_m3" | "baai/bge-m3" => Some(Self::BgeM3),
            "multilingual-e5-base" | "multilingual_e5_base" | "e5-base" | "e5" | "intfloat/multilingual-e5-base" => Some(Self::E5Base),
            "nomic-embed-text" | "nomic-embed-text-v1.5" | "nomic" => Some(Self::Nomic),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::BgeM3 => "bge-m3",
            Self::E5Base => "multilingual-e5-base",
            Self::Nomic => "nomic-embed-text-v1.5",
        }
    }

    pub fn repository(self) -> &'static str {
        match self {
            Self::BgeM3 => "BAAI/bge-m3",
            Self::E5Base => "intfloat/multilingual-e5-base",
            Self::Nomic => "nomic-ai/nomic-embed-text-v1.5",
        }
    }

    pub fn revision(self) -> &'static str {
        match self {
            Self::BgeM3 => "5617a9f61b028005a4858fdac845db406aefb181",
            Self::E5Base => "d128750597153bb5987e10b1c3493a34e5a4502a",
            Self::Nomic => "e9b6763023c676ca8431644204f50c2b100d9aab",
        }
    }

    /// The folder name under the model cache.
    pub fn directory(self) -> &'static str {
        self.id()
    }

    pub fn dimensions(self) -> usize {
        match self {
            Self::BgeM3 => 1024,
            Self::E5Base | Self::Nomic => 768,
        }
    }

    pub fn pooling(self) -> Pooling {
        match self {
            Self::BgeM3 => Pooling::Cls,
            Self::E5Base => Pooling::Mean,
            Self::Nomic => Pooling::Mean,
        }
    }

    pub fn document_prefix(self) -> &'static str {
        match self {
            Self::BgeM3 => "",
            Self::E5Base => "passage: ",
            Self::Nomic => "search_document: ",
        }
    }

    pub fn query_prefix(self) -> &'static str {
        match self {
            Self::BgeM3 => "",
            Self::E5Base => "query: ",
            Self::Nomic => "search_query: ",
        }
    }

    pub fn files(self) -> &'static [PinnedFile] {
        match self {
            Self::BgeM3 => &BGE_M3,
            Self::E5Base => &E5_BASE,
            Self::Nomic => &NOMIC,
        }
    }

    /// Which model a machine gets when `embed.model` is "auto": bge-m3 when a
    /// GPU is usable, else multilingual-e5-base (bge-m3's CPU index is far too
    /// slow to be the default).
    pub fn default_for(device: &Device) -> Self {
        if matches!(device, Device::Cuda(_)) { Self::BgeM3 } else { Self::E5Base }
    }
}

/// bge-m3: XLM-RoBERTa large; `pytorch_model.bin` is its only published weight
/// file, loaded with `VarBuilder::from_pth`.
pub const BGE_M3: [PinnedFile; 3] = [
    PinnedFile { name: "pytorch_model.bin", bytes: 2_271_145_830, sha256: "b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38" },
    PinnedFile { name: "tokenizer.json", bytes: 17_098_108, sha256: "21106b6d7dab2952c1d496fb21d5dc9db75c28ed361a05f5020bbba27810dd08" },
    PinnedFile { name: "config.json", bytes: 687, sha256: "26159e7ad065073448460117eb24b7a4572f6f4e78eadff65dc0a11c052449fa" },
];

/// multilingual-e5-base: XLM-RoBERTa base, safetensors.
pub const E5_BASE: [PinnedFile; 3] = [
    PinnedFile { name: "model.safetensors", bytes: 1_112_201_288, sha256: "a18a44fad1d0b46ded15928144138cff1135d5cc8233bdd90be5f18822de09a7" },
    PinnedFile { name: "tokenizer.json", bytes: 17_082_660, sha256: "62c24cdc13d4c9952d63718d6c9fa4c287974249e16b7ade6d5a85e7bbb75626" },
    PinnedFile { name: "config.json", bytes: 694, sha256: "9dab198f24c8c0879e481cf7822005d5ecbceedbacb390ffafa594e28d31bac4" },
];

/// nomic-embed-text-v1.5: the port's first model, for parity tests only.
pub const NOMIC: [PinnedFile; 3] = [
    PinnedFile { name: "model.safetensors", bytes: 546_938_168, sha256: "9e7d262b1fe5ea350782829496efa831901b77486bbde1cea54a4c822d010d5c" },
    PinnedFile { name: "tokenizer.json", bytes: 711_396, sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66" },
    PinnedFile { name: "config.json", bytes: 2_538, sha256: "9ab00bd92cee80a569f708140b7b6c1661a65891ff3765b1519e181ba2f2c92b" },
];

/// The device the embedder runs on, and what to say about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Cuda(String),
    Cpu(String),
}

impl DeviceKind {
    pub fn is_gpu(&self) -> bool {
        matches!(self, Self::Cuda(_))
    }

    pub fn label(&self) -> String {
        match self {
            Self::Cuda(name) => format!("GPU ({name})"),
            Self::Cpu(name) => format!("CPU ({name})"),
        }
    }
}

/// The best device this build (and machine) can use: `cuda` when the cargo
/// feature is on and a device initialises, else the CPU. The same rule decides
/// the default model.
pub fn device() -> (Device, DeviceKind) {
    #[cfg(feature = "cuda")]
    {
        if let Ok(device) = Device::cuda_if_available(0) {
            if matches!(device, Device::Cuda(_)) {
                let name = format!("{device:?}");
                return (device, DeviceKind::Cuda(name));
            }
        }
    }
    let name = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "cpu".into());
    (Device::Cpu, DeviceKind::Cpu(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_carry_their_dimensions_prefixes_and_pins() {
        assert_eq!(EmbeddingModel::BgeM3.dimensions(), 1024);
        assert_eq!(EmbeddingModel::BgeM3.pooling(), Pooling::Cls);
        assert_eq!(EmbeddingModel::BgeM3.document_prefix(), "");
        assert_eq!(EmbeddingModel::E5Base.dimensions(), 768);
        assert_eq!(EmbeddingModel::E5Base.pooling(), Pooling::Mean);
        assert_eq!(EmbeddingModel::E5Base.document_prefix(), "passage: ");
        assert_eq!(EmbeddingModel::E5Base.query_prefix(), "query: ");
        assert_eq!(EmbeddingModel::Nomic.document_prefix(), "search_document: ");
        for model in EmbeddingModel::ALL {
            assert_eq!(model.files().len(), 3, "{model:?}");
            for file in model.files() {
                assert_eq!(file.sha256.len(), 64);
                assert!(file.bytes > 0);
            }
        }
        // The names a lore.toml may carry.
        assert_eq!(EmbeddingModel::from_name("bge-m3"), Some(EmbeddingModel::BgeM3));
        assert_eq!(EmbeddingModel::from_name("e5"), Some(EmbeddingModel::E5Base));
        assert_eq!(EmbeddingModel::from_name("nomic"), Some(EmbeddingModel::Nomic));
        assert_eq!(EmbeddingModel::from_name("other"), None);
    }

    #[test]
    fn the_default_model_follows_the_device() {
        assert_eq!(EmbeddingModel::default_for(&Device::Cpu), EmbeddingModel::E5Base);
    }
}
