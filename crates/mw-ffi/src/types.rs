//! C ABI 境界で使う値型(初期構築仕様 §5.5)。
//!
//! `#[repr(...)]` の enum は csbindgen が C# 側 enum として自動生成する
//! (`build.rs` がこのファイルも入力に含める)。

/// `mw_sound_load` の `mode` 引数。
///
/// M1 は `Se`(全デコード常駐)のみ実装する。`Music`(圧縮のまま保持しストリーミング
/// デコード)は M2 で実装する — M1 では `MwResult::ErrUnsupportedSoundMode` を返す
/// (初期構築仕様 §5.2「SE はロード時に全デコードしてメモリ常駐。楽曲のみストリーミング
/// デコード」)。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwSoundMode {
    Se = 0,
    Music = 1,
}

impl MwSoundMode {
    /// FFI 境界から渡された生の `i32` を検証・復元する。範囲外は `None`。
    pub fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(MwSoundMode::Se),
            1 => Some(MwSoundMode::Music),
            _ => None,
        }
    }
}

/// 固定4本のバス識別子(`mw_core::BusId` の FFI 境界表現)。
///
/// 値は `mw_core::BusId` の `#[repr(u8)]` 判別子と1:1で一致させてある
/// (`from_core`/`to_core` は単純なキャストで往復できる)。
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MwBus {
    Master = 0,
    Bgm = 1,
    Se = 2,
    Voice = 3,
}

impl MwBus {
    pub fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(MwBus::Master),
            1 => Some(MwBus::Bgm),
            2 => Some(MwBus::Se),
            3 => Some(MwBus::Voice),
            _ => None,
        }
    }

    pub fn to_core(self) -> mw_core::BusId {
        match self {
            MwBus::Master => mw_core::BusId::Master,
            MwBus::Bgm => mw_core::BusId::Bgm,
            MwBus::Se => mw_core::BusId::Se,
            MwBus::Voice => mw_core::BusId::Voice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_mode_from_raw_round_trips_and_rejects_unknown() {
        assert_eq!(MwSoundMode::from_raw(0), Some(MwSoundMode::Se));
        assert_eq!(MwSoundMode::from_raw(1), Some(MwSoundMode::Music));
        assert_eq!(MwSoundMode::from_raw(2), None);
        assert_eq!(MwSoundMode::from_raw(-1), None);
    }

    #[test]
    fn bus_from_raw_round_trips_and_rejects_unknown() {
        assert_eq!(MwBus::from_raw(0), Some(MwBus::Master));
        assert_eq!(MwBus::from_raw(3), Some(MwBus::Voice));
        assert_eq!(MwBus::from_raw(4), None);
    }

    #[test]
    fn bus_to_core_matches_discriminant() {
        assert_eq!(MwBus::Master.to_core() as u8, 0);
        assert_eq!(MwBus::Voice.to_core() as u8, 3);
    }
}
