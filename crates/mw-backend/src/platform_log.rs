//! 診断ログの出力先を OS ごとに切り替える。
//!
//! Android では**プロセスの stderr はどこにも出ない**(`log.redirect-stdio` は
//! user ビルドの SELinux ポリシーで設定できない)。そのため `eprintln!` で書いた
//! 診断は実機で1行も読めず、`docs/measurement-m1.md` §8.6-1 の
//! 「起動ログから絶対値を見積もる」手段が Android では使えなかった。
//! ここで logcat(`liblog` の `__android_log_write`)へ振り分ける。
//!
//! Apple(iOS / macOS)では stderr に加えて unified logging にも同じ1行を出す。
//! Xcode / devicectl の `--console` につながずに起動した回でも、あとから
//! `log collect` / `log show` で読めるようにするため。`os_log` 系は C の可変長マクロで
//! FFI から直接呼べないため、libSystem の `syslog(3)` を `"%s"` 書式で呼ぶ
//! (メッセージは引数として渡すので書式として解釈されず、`<private>` にも伏せられない)。
//!
//! **ゲームスレッドから呼ぶこと。** `format!` のヒープアロケーションと
//! stderr / logcat のロックを伴うため、音声コールバック経路では使えない
//! (`crates/mw-core/COMMON.md` のリアルタイム安全性規約)。これは
//! 置き換え前の `eprintln!` と同じ制約で、新たな制約は増えていない。

/// 1行の診断ログを出す。Android は logcat(タグ `mw`)、Apple は stderr と unified logging、
/// それ以外は stderr。
pub fn write(message: &str) {
    #[cfg(target_os = "android")]
    android::write(message);

    #[cfg(not(target_os = "android"))]
    eprintln!("{message}");

    #[cfg(target_vendor = "apple")]
    apple::write(message);
}

/// `eprintln!` と同じ書式で診断ログを出す(出力先は [`write`] が OS ごとに振り分ける)。
///
/// `mw-ffi` からも使うため `#[macro_export]`(クレート直下 `mw_backend::mw_log!` で参照できる)。
#[macro_export]
macro_rules! mw_log {
    ($($arg:tt)*) => {
        $crate::platform_log::write(&format!($($arg)*))
    };
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    /// `android/log.h` の `ANDROID_LOG_INFO`。
    const ANDROID_LOG_INFO: c_int = 4;

    /// logcat のタグ。`adb logcat -s mw:V` で絞り込める。
    const TAG: &str = "mw";

    // NDK の liblog。cargo-ndk のリンカ設定でそのまま解決できる(追加の依存クレートは不要)。
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub fn write(message: &str) {
        // メッセージ中の NUL は CString が弾く。診断ログのために panic はしない。
        let (Ok(tag), Ok(text)) = (CString::new(TAG), CString::new(message)) else {
            return;
        };

        // SAFETY: tag / text はいずれも NUL 終端された有効なポインタで、呼び出しの間だけ生存する。
        unsafe {
            __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr());
        }
    }
}

#[cfg(target_vendor = "apple")]
mod apple {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    /// `syslog.h` の `LOG_NOTICE`(unified logging では default レベルで残る)。
    const LOG_NOTICE: c_int = 5;

    unsafe extern "C" {
        fn syslog(priority: c_int, format: *const c_char, ...);
    }

    pub fn write(message: &str) {
        // メッセージ中の NUL は CString が弾く。診断ログのために panic はしない。
        let (Ok(format), Ok(text)) = (CString::new("%s"), CString::new(message)) else {
            return;
        };

        // SAFETY: format / text はいずれも NUL 終端された有効なポインタで、呼び出しの間だけ生存する。
        // 書式は固定の "%s" で、メッセージは引数として渡す。
        unsafe {
            syslog(LOG_NOTICE, format.as_ptr(), text.as_ptr());
        }
    }
}

#[cfg(all(test, target_vendor = "apple"))]
mod apple_tests {
    #[test]
    fn write_does_not_panic_on_odd_input() {
        super::write("mw-log-probe %s %d 100%");
        super::write("nul\0inside");
    }
}
