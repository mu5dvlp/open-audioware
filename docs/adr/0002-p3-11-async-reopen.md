# 0002. 内部再オープン(P3-11)はレジストリの `Mutex` を保持したまま行わず、3段構成へ分割する

## 状態

採用

## 日付

2026-09-23

## 文脈

Android(AAudio)切断からの内部再オープン(初期構築仕様『§6』案A)は、`handle_registry::
maybe_reopen`(`mw_poll_events` から毎フレーム呼ばれる)がレジストリの `Mutex` を保持した
まま `Instance::attempt_reopen` を丸ごと実行していた。その中身は `backend.close()` →
`stop_and_join_decode_threads()`(スレッド2本を join。待ち時間に上限無し)→
`Renderer::build_with_events()` → `backend.open()`(cpal のデバイスオープン。数百 ms
かかりうる)→ `SymphoniaDecoder::open_shared`(デコード probe)。この間、`with_instance`
経由の他のすべての FFI 呼び出しが同じ `Mutex` を取ろうとしてゲームスレッドで足止めされる
——初期構築仕様『§5.4「全関数非ブロッキング」』への最悪の違反であり、
`docs/plans/REFACTOR-PLAN.md` の P3-11 として長く「設計だけ合意し、着手は保留」の
状態だった(素朴にロックを早く手放すと「バックエンドの無い `Instance`」を他の呼び出しが
見てしまう、という危険がある design memo が先に残されていた)。

## 決定

**再オープン中の API 呼び出しは「成功を返して捨てる」**(オーケストレータが事前に決定。
専用のエラーコードは作らない、ブロックもしない)。この方針を成立させるため、再オープンを
3段構成へ分割し、**`Instance` はレジストリに存在し続けたまま、重い部品(バックエンド・
デコードスレッド)だけを一時的に切り離す**設計にした:

1. **段1**(`Instance::begin_reopen`。レジストリの `Mutex` を保持中): 復元用スナップショットを
   読み取り、`std::mem::replace(&mut instance.backend, CpalBackend::new())` で新品の
   未オープンバックエンドに差し替え、`decode_thread`/`bgm_decode_thread` の `JoinHandle` を
   `take()` する。ロックを保持したまま行うのはここまで——いずれも安い代入・take のみ。
2. **段2**(`run_reopen_worker`。専用のワーカースレッド上、ロック無し): 切り離した旧
   バックエンドの `close()`、旧デコードスレッド2本の停止・`join()`、新しい `Renderer` 一式の
   構築、新バックエンドの `open()`、新デコードスレッド2本の起動。**時間のかかる処理は
   すべてここに集約され、レジストリの `Mutex` を一切取らない。**
3. **段3**(`finalize_reopen_success`/`finalize_reopen_failure`。レジストリの `Mutex` を
   再度取得): 段2 の結果を `Instance` へ差し込み、状態を復元する。**`Instance` の `handle`
   が段1で捕まえたものとまだ一致する場合に限る**——一致しなければ(段2の間に `mw_shutdown`
   が来ていた/shutdown→init で別インスタンスに変わっていた)、`teardown_orphaned_reopen`
   が段2で組み立てた新バックエンド・新デコードスレッドを自分で畳んで捨てる。レジストリには
   一切触れない。

二重起動を防ぐため、`Instance` のフィールドではなくモジュール `static`
`REOPEN_IN_PROGRESS: AtomicBool` を追加した(`ReopenPolicy` 自体は「1回の試行の結果」を
段3でしか記録しないため、段2実行中は `is_due()` が `true` のままになりうる——`Instance`の
フィールドにすると、それを読むためにレジストリの `Mutex` が要ってしまい、目的〔ロック無しの
安価な事前チェック〕を果たせない)。`ReopenInProgressGuard`(RAII)が段2〜段3の間ずっと
立てておき、`Drop` で必ず解除する——`panic = "unwind"` のままなので、段2内のパニックでも
スタック巻き戻しでこの `Drop` は必ず走る。

## 理由

- **既存の25箇所の FFI 呼び出しサイト(`ffi.rs`)を1つも変えずに済む。** `Instance` が
  レジストリに存在し続けるため、`with_instance` は段2の間も同じ `Instance` を見つける:
  - getter 系は `self.backend`(段2の間は未オープンの `CpalBackend`)を読むだけなので、
    `Backend` トレイトが元々持つ「未オープンなら 0」の契約にそのまま乗る。
  - コマンド系は `self.command_sender`(畳まれつつある旧 `Renderer` の受信側)へ積むだけ
    なので、rtrb の `Producer::push` は相手が消えていても即座にはエラーにならず、
    容量に余裕がある限り `Ok` を返しつつ実際には誰にも読まれず捨てられる——
    「成功を返して捨てる」がそのまま実現される。
- ロックを早期に手放しつつ `Instance` を有効なまま保つことで、「バックエンドの無い
  `Instance` を他の呼び出しが見てしまう」という当初の懸念を、新しいエラー処理を増やさずに
  解消できた。

## 影響・トレードオフ

- **`Instance::backend_sample_rate()`**(`mw_sound_load` のリサンプル判定、初期構築仕様
  『§4.7』)は段2の間 `self.backend.sample_rate() == 0` になる。`last_known_sample_rate`
  (`AtomicU32`)へフォールバックすることで解決したが、デバイスのレートが再オープンを挟んで
  実際に変わっていた場合、その窓でロードされた SE は「1つ前の」レートを前提にリサンプル
  される(クラッシュはしない、再ロードすれば直る、窓は数百 ms)。
- **`mw_shutdown`**: 段2の間に呼ばれると `instance.backend.close()` が
  `BackendError::NotOpen` を返す(段1が置いた未オープンのバックエンドのため)。これを
  `CloseFailed` として報告すると誤解を招くため、`is_reopen_in_progress()` が `true` の間は
  `Closed` として報告するよう変更した。再オープンが**失敗**した直後(バックオフ待機中)の
  `NotOpen` は、この変更以前から存在する挙動のため、従来通り `CloseFailed` のまま扱う。
- **既知の残り**: `mw_music_set`/`mw_bgm_set` が使う `decoder_tx`/`bgm_decoder_tx`
  (`mpsc::Sender`)は、旧デコードスレッドが段2で実際に `join()` され切った後は `send` が
  失敗し `MwResult::ErrCommandQueueFull` を返す——rtrb のコマンドキューと異なり
  「相手が消えても即座にはエラーにならない」わけではないため。ただしこれは今回の分割が
  新たに持ち込んだものではなく、再オープンが失敗した後の自動再試行までの待機中にも
  以前から起きていた挙動であり、ブロックしない・新しいエラーコードを増やさないという
  性質は変わらないため、対症療法(段2専用のダミー受信先への差し替え等)は見送った。
- **受け入れ確認には Android 実機が要る**(AAudio の切断→復帰)。`cargo test` は
  「段2の間、他の呼び出しがブロックされないこと」自体を測れない——CI にはデバイスが
  無く `backend.open()` が即座に失敗して終わるため、直したい待ち時間そのものが発生しない。
