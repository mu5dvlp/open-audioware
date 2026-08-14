//! バス(初期構築仕様 §4.1, 【仮】): Master / BGM / SE / Voice の固定4本。
//!
//! 各バスは音量(linear)+ フェード(目標値+時間、サンプル単位の線形ランプ)を持つ。
//! 任意本数のバスツリーは作らない(音ゲーに不要)。

use crate::ramp::Ramp;

/// 固定4本のバス識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BusId {
    Master = 0,
    Bgm = 1,
    Se = 2,
    Voice = 3,
}

/// バスの総数(固定)。
pub const BUS_COUNT: usize = 4;

/// 全バスの列挙(固定順。`BusSet` の内部配列インデックスと一致させる)。
pub const ALL_BUSES: [BusId; BUS_COUNT] = [BusId::Master, BusId::Bgm, BusId::Se, BusId::Voice];

impl BusId {
    /// `BusSet` 内部配列へのインデックス。
    pub const fn index(self) -> usize {
        self as usize
    }

    /// `u8` から復元する(FFI 境界からの整数入力の検証用)。範囲外は `None`。
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(BusId::Master),
            1 => Some(BusId::Bgm),
            2 => Some(BusId::Se),
            3 => Some(BusId::Voice),
            _ => None,
        }
    }
}

/// 1本のバスの状態。音量は常にランプ経由(即値変更もランプ 0 サンプルとして表現できるが、
/// M13 の既定に従い、外部からの「即時変更」要求も既定 5ms のランプを通す — その変換は
/// `Mixer` がコマンド処理時に行う。ここでは `Ramp` をそのまま保持するだけ)。
#[derive(Debug, Clone, Copy)]
pub struct Bus {
    pub volume: Ramp,
}

impl Bus {
    pub const fn new() -> Self {
        Self {
            volume: Ramp::new(1.0),
        }
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

/// 固定4本のバスをまとめて保持する。
#[derive(Debug, Clone, Copy)]
pub struct BusSet {
    buses: [Bus; BUS_COUNT],
}

impl BusSet {
    pub const fn new() -> Self {
        Self {
            buses: [Bus::new(); BUS_COUNT],
        }
    }

    pub fn get(&self, id: BusId) -> &Bus {
        &self.buses[id.index()]
    }

    pub fn get_mut(&mut self, id: BusId) -> &mut Bus {
        &mut self.buses[id.index()]
    }
}

impl Default for BusSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_buses_default_to_unity_volume() {
        let set = BusSet::new();
        for id in ALL_BUSES {
            assert_eq!(set.get(id).volume.value(), 1.0);
        }
    }

    #[test]
    fn from_u8_round_trips_for_valid_ids() {
        for id in ALL_BUSES {
            assert_eq!(BusId::from_u8(id as u8), Some(id));
        }
        assert_eq!(BusId::from_u8(4), None);
    }
}
