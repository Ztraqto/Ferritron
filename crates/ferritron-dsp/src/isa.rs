//! Run-time choice of the instruction set. `process` code must not call [`isa`]: pick a
//! function pointer once (in `activate`) and call that.

use std::sync::OnceLock;

/// Instruction-set levels. x86_64 levels are ordered; `Neon` belongs to aarch64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    Scalar,
    Sse2,
    Sse41,
    Avx2,
    Neon,
}

impl Isa {
    fn family_rank(self) -> (u8, u8) {
        match self {
            Isa::Scalar => (0, 0),
            Isa::Sse2 => (1, 1),
            Isa::Sse41 => (1, 2),
            Isa::Avx2 => (1, 3),
            Isa::Neon => (2, 1),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Isa::Scalar => "scalar",
            Isa::Sse2 => "sse2",
            Isa::Sse41 => "sse41",
            Isa::Avx2 => "avx2",
            Isa::Neon => "neon",
        }
    }

    pub fn parse(s: &str) -> Option<Isa> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scalar" => Some(Isa::Scalar),
            "sse2" => Some(Isa::Sse2),
            "sse41" | "sse4.1" => Some(Isa::Sse41),
            "avx2" => Some(Isa::Avx2),
            "neon" => Some(Isa::Neon),
            _ => None,
        }
    }

    /// True when code written for `needed` can run at this level.
    pub fn supports(self, needed: Isa) -> bool {
        let (fa, ra) = self.family_rank();
        let (fb, rb) = needed.family_rank();
        needed == Isa::Scalar || (fa == fb && ra >= rb)
    }
}

/// Detects what the CPU can do, ignoring the environment.
pub fn detect() -> Isa {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            Isa::Avx2
        } else if is_x86_feature_detected!("sse4.1") {
            Isa::Sse41
        } else {
            Isa::Sse2
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        Isa::Neon
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Isa::Scalar
    }
}

/// Applies a `FERRITRON_ISA` request to a detected level: it can only lower the level.
/// Unknown names and levels of another architecture are ignored.
pub fn resolve(detected: Isa, request: Option<&str>) -> Isa {
    match request.and_then(Isa::parse) {
        Some(r) if detected.supports(r) => r,
        _ => detected,
    }
}

/// The level in use: detected once, lowered by the environment variable `FERRITRON_ISA`
/// (`scalar`, `sse2`, `sse41`, `avx2`, `neon`).
pub fn isa() -> Isa {
    static ISA: OnceLock<Isa> = OnceLock::new();
    *ISA.get_or_init(|| resolve(detect(), std::env::var("FERRITRON_ISA").ok().as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_request_only_lowers() {
        assert_eq!(resolve(Isa::Avx2, Some("sse2")), Isa::Sse2);
        assert_eq!(resolve(Isa::Avx2, Some("scalar")), Isa::Scalar);
        assert_eq!(resolve(Isa::Sse2, Some("avx2")), Isa::Sse2);
        assert_eq!(resolve(Isa::Avx2, Some("neon")), Isa::Avx2);
        assert_eq!(resolve(Isa::Neon, Some("sse2")), Isa::Neon);
        assert_eq!(resolve(Isa::Avx2, Some("bogus")), Isa::Avx2);
        assert_eq!(resolve(Isa::Avx2, None), Isa::Avx2);
        assert_eq!(resolve(Isa::Sse41, Some("SSE41")), Isa::Sse41);
    }

    #[test]
    fn detected_is_usable() {
        let d = detect();
        assert!(d.supports(Isa::Scalar));
        #[cfg(target_arch = "x86_64")]
        assert!(d.supports(Isa::Sse2));
        assert!(isa().supports(Isa::Scalar));
    }
}
