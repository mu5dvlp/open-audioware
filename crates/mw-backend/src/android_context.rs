//! Android の JavaVM / Context を `ndk_context` へ登録する。
//!
//! cpal 0.18 の AAudio 実装は、デバイス列挙・既定構成の取得・ストリーム生成のいずれからも
//! **Java 側の `AudioManager` / `PackageManager`** を参照する。その入口が `ndk_context` で、
//! `ndk-glue` や `android-activity` を使うアプリでは自動的に初期化されるが、
//! **Unity のようなホストアプリのプロセスでは誰も初期化しない**。未初期化のまま cpal を呼ぶと
//! `android context was not initialized` で panic し、`mw_init` が `ErrPanic` を返して
//! ミドルウェアが一切動かない(2026-08-23 に実機で確認。docs/measurement-m1.md §6.2)。
//!
//! そこで:
//!
//! 1. `JNI_OnLoad`(`mw-ffi` が公開)で JavaVM を受け取り [`set_java_vm`] で控える
//! 2. バックエンドを開く直前に [`ensure_initialized`] を呼び、`ActivityThread` 経由で
//!    Application Context を取って `ndk_context` へ登録する
//!
//! Context は `ActivityThread.currentActivityThread().getApplication()` で取る。Activity への
//! 参照を要求しないので**ホスト側に一切の協力を求めない**(`.so` を置くだけで動く)。

use std::ffi::c_void;
use std::ptr;
use std::sync::Once;
use std::sync::atomic::{AtomicPtr, Ordering};

use jni::{JavaVM, jni_sig, jni_str};

static JAVA_VM: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static INIT: Once = Once::new();

/// `JNI_OnLoad` から JavaVM を控える。ライブラリのロード時に1度だけ呼ばれる。
pub fn set_java_vm(vm: *mut c_void) {
    JAVA_VM.store(vm, Ordering::Release);
}

/// `ndk_context` を1度だけ初期化する。失敗してもログを残すだけで panic はしない
/// (cpal 側が後段で panic するが、それは `mw_init` の `catch_unwind` が畳む)。
pub fn ensure_initialized() {
    INIT.call_once(|| {
        let vm_ptr = JAVA_VM.load(Ordering::Acquire);
        if vm_ptr.is_null() {
            crate::mw_log!(
                "[mw-backend] android: JavaVM を持っていない(JNI_OnLoad が呼ばれていない)。\
                 ndk_context を初期化できない"
            );
            return;
        }

        // SAFETY: JNI_OnLoad が渡してきた JavaVM ポインタ。プロセスの寿命いっぱい有効。
        let vm = unsafe { JavaVM::from_raw(vm_ptr.cast()) };

        let context = vm.attach_current_thread(|env| {
            let thread = env
                .call_static_method(
                    jni_str!("android/app/ActivityThread"),
                    jni_str!("currentActivityThread"),
                    jni_sig!("()Landroid/app/ActivityThread;"),
                    &[],
                )?
                .l()?;
            let application = env
                .call_method(
                    &thread,
                    jni_str!("getApplication"),
                    jni_sig!("()Landroid/app/Application;"),
                    &[],
                )?
                .l()?;

            // グローバル参照にしてから `into_raw` で手放す(解放しない)。Context は
            // プロセスの寿命いっぱい生かす必要があり、`ndk_context` も解放しないため。
            let global = env.new_global_ref(&application)?;
            Ok::<_, jni::errors::Error>(global.into_raw())
        });

        match context {
            Ok(context) => {
                // SAFETY: vm はプロセス寿命で有効、context はグローバル参照(解放しない)。
                // `Once` により初期化は高々1回。
                unsafe { ndk_context::initialize_android_context(vm_ptr, context.cast()) };
                crate::mw_log!("[mw-backend] android: ndk_context を初期化した");
            }
            Err(err) => {
                crate::mw_log!("[mw-backend] android: Application Context の取得に失敗した: {err}");
            }
        }
    });
}
