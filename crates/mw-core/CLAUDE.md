# mw-core

OS 非依存・デバイス非依存のコア。ミキサ、ボイス管理、クロック、デコード、リサンプルを置く場所。
`mw-backend` にも `mw-ffi` にも依存しない(依存の向きは常に内向き。ワークスペース全体の方針は
`../../CLAUDE.md` を参照)。

オフラインレンダリング(デバイス無しでバッファに書き出す)はこのクレートだけで完結させる。
テストの主戦場はここになる。

## 現状(M1: SE 再生)

- `event`: `Event`(§4.6 イベント通知。`RouteChanged`/`Underrun`/`MusicEnded`/
  `MusicLooped`/`StreamError`/`ClipperEngaged`/`AudioInterruptionBegan`/
  `AudioInterruptionEnded`(M3)の8種。`AudioInterruptionBegan`/`AudioInterruptionEnded`
  は元々 `crates/mw-backend/src/ios_interruption.rs`(iOS/tvOS)専用に追加したが、
  `AudioInterruptionEnded { recovered }` は M3 完了後に `crates/mw-ffi/src/handle.rs`
  の Android(AAudio)内部再オープンの成否通知にも転用されている(iOS/tvOS と
  それ以外でどちらか一方の経路しかコンパイルされないため競合しない。理由は
  `Event::AudioInterruptionEnded` 自身のドキュメント参照)。
  可変長データは持たず、バリアントごとの付随データは固定サイズの数値のみ)と
  `EventQueue`(固定容量、【仮】既定64。溢れたら古いものから破棄し、破棄数を読み手側で
  逆算する、M2-6)。
  書き込み経路が2系統ある: 音声スレッド(唯一の書き手 `Mixer`)専用の
  `push_realtime`(ロック・アロケーション無し。各スロットを1本の `AtomicU64` に
  タグ+ペイロードで詰め、`write_index` の Release/Acquire だけで公開する——
  `clock.rs::MusicClockPublisher` のような seqlock は要らない設計にした理由は
  `event.rs` モジュール doc を参照)と、非リアルタイムスレッド(cpal のエラー
  コールバック等)専用の `push_side_channel`(§5.3 の対象外なので `Mutex` を使う)。
  読み出し `drain` はゲームスレッド(`mw-ffi::mw_poll_events`)から呼ぶ。
- `format`: 内部ミックスフォーマット定義。f32 ステレオ固定(`CHANNELS = 2`)、
  サンプルレートは出力デバイスに追従(将来変わりうる前提は `AudioFormat` に閉じ込めてある)。
- `clock`: `RenderedFrameCounter`(音声コールバックが送出したフレーム数を数える
  `AtomicU64` カウンタ)と `MusicClockPublisher`(§4.4 の心臓部。曲位置 × ホスト単調時刻の
  相関点・世代カウンタ・楽曲状態(`state: MusicState`)・`is_playing` を seqlock で
  公開する。書き手は音声スレッドの `Mixer::render` のみ、読み手はロック無しで任意スレッドから
  `snapshot()` できる。Release/Acquire が片方向の制約にしかならない理由は
  `write`/`snapshot` のコメントに必ず残してある——読み飛ばして単純な
  `Ordering::Release`/`Acquire` ロードに戻さないこと)。`state` は `mw_music_state()`
  (§5.5)の実体そのもの——`MusicVoice::state()` は音声スレッド排他所有の `Mixer` の
  内部にしかないため、ゲームスレッドから読める唯一の経路がこの seqlock 経由になる
  (M2-7)。`MusicState` 自体は atomic に乗らないので `MusicState::to_u8`/`from_u8`
  で数値表現に変換してから `AtomicU8` へ格納する。`is_playing` は `state ==
  MusicState::Playing` から `publish` の中で導出する冗長フィールド(食い違うスナップショットが
  観測されないよう、必ず同じ seqlock 書き込み区間の中で `state` と一緒に計算する。
  残してある理由は `MusicClockSnapshot::is_playing` のコメント参照)。
  `mixer::build` が返す `Arc<MusicClockPublisher>` を経由してゲームスレッド側へ渡す(M2-5)。
  `BgmStatePublisher`(M4-3): BGM ボイス(下記)の状態だけをロック無しで公開する、
  `MusicClockPublisher` よりずっと軽い publisher。BGM は**クロックを持たない**
  (初期構築仕様『§2』M14: 発行元は楽曲ボイスに固定)ため、公開すべき値は
  `MusicState` 1個だけで済み、複数フィールドの整合を取る seqlock は不要——単一の
  `AtomicU8`(`MusicState::to_u8`/`from_u8` を再利用)の Release ストア/Acquire
  ロードだけで書き手・読み手の整合が保証できる(`RenderedFrameCounter` と同じ
  「単一フィールドは単純な atomic で足りる」考え方)。
- `schedule`: 予約発音(§4.5)のソート済みキュー `ScheduleQueue<T>` と、
  ホスト時刻→バッファ内オフセットの変換(`offset_within_buffer`。切り捨て、
  丸め方向の根拠はモジュール doc を参照)。SE 予約はここのキューに、楽曲の予約再生は
  単一スロット(`mixer.rs::MusicSchedule`)に積む(楽曲ボイスは同時に1本なので
  キューが要らない)。`Mixer::render` から呼ばれるため固定容量・アロケーション無し(M2-5)。
- `music` / `stream`: `MusicVoice`(唯一の楽曲ボイス。状態機械は `Loading`/`Ready`/
  `Playing`/`Paused`、M2-2)と、その PCM 供給元 `StreamingMusicSource`/
  `MusicStreamProducer`(SPSC リングバッファ越しのストリーミングデコード連携。
  シークはエポック ack 方式で調停する、M2-3)。M2-5 で `Mixer` へ実際に組み込んだ
  (`Mixer::render` が毎コールバック `MusicVoice::render` を呼ぶ)。デコードスレッドを
  起動して `MusicStreamProducer::pump` を回す側(mw-ffi の後続作業)と、
  楽曲ロード FFI(`mw_music_set` 相当)はまだ無い——`mixer::build` は内部で
  `stream::channel` を組み立てて返すが、誰も `pump` しない限り楽曲ボイスは
  `Loading` のまま(M2-5 時点の正直な現状)。
- `decode`: `MusicDecoder`/`SymphoniaDecoder` — wav / ogg vorbis のストリーミングデコード
  (§4.7, M2-3)。`stream.rs::MusicStreamProducer::pump` から呼ばれる。
- `config`: `Config` — 【仮】既定値(ボイス数 64、既定ランプ 5ms、キュー容量、
  予約発音キュー容量32、イベントキュー容量64、アンダーラン集約報告閾値48000フレーム等)
  を1箇所に集約する設定構造体。`mw_init(config)` からの実行時上書きは未実装(既定値のみ)。
- `ramp`: `Ramp` — サンプル単位の線形ランプ。全ての音量変化・停止がこれを経由する(M13)。
  `advance()` は音声コールバックのホットパスから呼ばれる(`Iterator::next` と紛らわしいため
  意図的に別名にしてある)。`ms_to_samples` でミリ秒→サンプル数を変換する。
- `bus`: `BusId`(Master/Bgm/Se/Voice の固定4本)、`Bus`(音量 = `Ramp`)、`BusSet`。
- `clipper`: `SoftClipper` — Master 段のソフトクリッパ。閾値以下は完全に透過(線形)、
  超過分だけ `excess / (1 + excess)` で滑らかに飽和させる。動作回数を `AtomicU64` で
  カウントする(開発ビルドでの動作検知。イベント通知への昇格は M3)。
- `sound`: `SoundData`(f32 ステレオ・インターリーブ PCM)、`SoundId`、`SoundStorage`
  (ID 管理。ゲームスレッド専用、ヒープアロケーションを伴うため音声コールバック経路からは
  絶対に使わない)。
- `wav`: 16bit PCM / モノラル・ステレオの wav を自前パーサでデコードする。
  モノは等パワー(`1/√2`)で両ch展開。サンプルレートは出力デバイスと一致しなくてよく、
  一致しない場合は `resample.rs`(rubato `SincFixedIn`)でロード時に一括変換して
  出力レート化する(§4.7, M2)。非対応フォーマットは `WavError` の具体的なバリアントで返す。
  `#[cfg(test)] pub mod golden` にゴールデンテスト用の wav バイト列ビルダを置く
  (バイナリはコミットしない。§8)。
- `resample`: サンプルレート変換(§4.7, M2)。楽曲ストリーミング用の `StreamResampler`
  (`rubato::FftFixedInOut`。`decode.rs::SymphoniaDecoder` が1曲につき1個だけ保持し
  `pump()` をまたいで使い回すことでブロック境界の連続性を保つ)と、SE ロード時一括変換の
  `resample_oneshot`(`rubato::SincFixedIn`、高品質設定)の2系統。レート一致時はどちらも
  リサンプラを構築せずバイパスする。総フレーム数・シーク位置は常に出力レート基準
  (`convert_frame_count` に一本化)。
- `voice`: `VoicePool` — 固定容量のボイスプール(既定 64 primary + 64 tail)。
  枯渇時は最古のボイスを「尾」スロットへ退避しつつ既定ランプでフェードアウトさせ、
  元のスロットへ新規ボイスを即座に割り当てる(スティール。§4.2)。停止・スティールは
  必ずランプを経由し、自然終了(PCM 終端到達)はランプ不要としてただちに回収する。
- `command`: `Command` — ゲームスレッド→音声スレッドのコマンド列挙
  (`PlaySe` / `StopVoice` / `SetVoiceVolume` / `StopVoicesUsingSound` /
  `SetBusVolume` / `BusFade` / `SeSchedule` / `MusicPlayScheduled` / `MusicPrepare` /
  `MusicSeek` / `MusicPause` / `MusicResumeAt` / `MusicStop` / `MusicSetLoop` /
  `BgmPrepare` / `BgmSeek` / `BgmPlay` / `BgmStop` / `BgmSetLoop`)。
  `SeSchedule`〜`MusicSeek` は M2-5 追加(予約発音・楽曲予約再生・楽曲シーク)。
  楽曲制御4種(`MusicPause`/`MusicResumeAt`/`MusicStop`/`MusicSetLoop`)は M2-7 前半で
  追加——§4.3 の楽曲制御 API を mw-ffi へ公開する下ごしらえとして、`MusicVoice` 側に
  既に実装済みだった `pause`/`resume_at`/`stop`/`set_loop` への配線をここで揃えた
  (`MusicVoice` 自体の変更は無し)。いずれも不連続の発生源(シーク・巻き戻し再開・
  停止)は `MusicRenderOutcome::discontinuity` 経由で `clock.rs` の世代カウンタへ
  そのまま乗るため、`mixer.rs::Mixer::render` 側に追加配線は要らなかった。
  `MusicPrepare` は M2-7 後半(`mw_music_set` の実装)で追加——同様に `MusicVoice`
  側に既にあった `prepare`(新しい曲として `Loading` から仕切り直す)への配線を
  足しただけ。mw-ffi の `mw_music_set` が「デコーダ差し替え → `MusicPrepare` →
  `MusicSeek{0}`」の順でコマンドを送ることで、前の曲が `Playing` 中でも状態機械・
  ゲイン・リングバッファの3つを矛盾なく初期化できる。`MusicPrepare` の直後に
  `MusicStop` を挟む必要は無い(挟んでも安全だが完全な no-op になるだけ)ため、
  現在の実装では省かれている(`command.rs::Command::MusicPrepare` のドキュメント、
  `mixer.rs` の
  `music_set_command_sequence_recovers_cleanly_from_a_song_still_playing` テスト参照)。
- `mixer`: `Mixer::render(output, buffer_start_host_time_ns)` — 「コマンド消化 →
  楽曲ボイスのレンダリング(予約発火があればサンプル精度で分割) → BGM ボイスの
  レンダリング(M4-3, 下記) → イベント通知 → アクティブ SE ボイス合算 + 楽曲・BGM の
  Bgm バス適用 → Master → クリッパ → 音楽クロックの相関点 + BGM 状態を公開」
  (§4.1/§4.4/§4.6, M14)。`mixer::build(config, sample_rate)` が `Mixer` と
  ゲームスレッド側ハンドル一式(`CommandSender` / `ReclaimReceiver` /
  `MusicStreamProducer` / `Arc<MusicClockPublisher>` / `Arc<EventQueue>` /
  `BgmHandles { stream_producer, state }`)を返す。
  `buffer_start_host_time_ns` はこのバッファの先頭フレームが実際に DAC から
  出力される(と予測される)ホスト単調時刻——`mw-backend` が cpal の
  `OutputCallbackInfo::timestamp().playback` から求めて渡す(M2-5)。
  `MusicRenderOutcome::ended`/`looped`/`underrun_frames` をそのままイベント化する
  (新たな検知ロジックは足していない、M2-6)。`report_underrun` がアンダーランの
  集約(コールバックをまたいで蓄積し、収まるか閾値到達で1件にまとめる)を行う理由は
  同メソッドのコメントを参照。クリッパ動作検知(`ClipperEngaged`)は開発ビルドのみ
  (`cfg!(debug_assertions)`)発火する。
  **M4-3(BGM 用の2本目の楽曲ボイス、初期構築仕様『§2』M14)**: `bgm_voice`
  フィールドは `music_voice` と**同じ `MusicVoice` 型を転用**したもの(状態機械・
  フェード・ループの実装をまるごと共有できる)。「クロックを持たない」という M14 の
  要件は `MusicVoice` 自身の責務ではなく、その位置を `music_clock`
  (`MusicClockPublisher`)へ**公開しない**という `Mixer::render` 側の配線だけで
  満たしている——BGM の状態は代わりに軽量な `BgmStatePublisher`(`clock.rs`)で
  公開する。BGM PCM の供給元(`bgm_source: StreamingMusicSource`)は
  `music_source` とは独立した、もう一組の `stream::channel` インスタンス
  (リングバッファもデコード進行も完全に別)。予約再生・巻き戻し付き再開は BGM に
  無い(不要なため対応コマンドを持たせていない)。`output` は既に楽曲の生 PCM
  保持に使っているため BGM 用に二重利用できず、かつヒープ確保もできない(§5.3)ので、
  固定長のスタック配列(`BGM_CHUNK_FRAMES = 256` フレームぶん、【仮】)へチャンク
  単位で読み直しながら周波数ループの中で都度リフィルする設計にしてある
  (`Mixer::render` の `BGM_CHUNK_FRAMES` ドキュメント参照)。楽曲・BGM の両ボイスは
  **同じ Bgm バスを共有**する(新しい専用バスは追加しない)——両者が同時に鳴っても
  単純に加算されるだけで壊れず、これにより「画面遷移をまたぐクロスフェード」が
  各ボイス自身のフェード(独立したゲイン)だけで自然に成立する。
- `renderer`: `Renderer` — `Mixer` を包み、レンダリング済みフレーム数を数える最上位型。
  `Renderer::build` が `mixer::build` を呼ぶ薄いラッパ(戻り値もそのまま中継する)。

## 所有権モデル(M1 で確定させた設計)

M0 では `Arc<Renderer>` をゲームスレッドと音声スレッドで共有していたが、M1 では
**`Renderer`(および内部の `Mixer`)は音声コールバックスレッドの排他所有物**とし、
`mw-backend::Backend::open` へ値渡し(ムーブ)する設計にした。ゲームスレッドは
以下の2つの別ハンドルを経由してのみ音声スレッドとやり取りする:

- `CommandSender`(`build` が返す): `rtrb::Producer<Command>` を `Mutex` で包んだもの。
  ゲームスレッド側は複数スレッドから並行に呼ばれうる(§5.4)ため `Mutex` を使ってよい
  (§5.3 が禁止するのは音声スレッド側でのロック取得のみ)。
- `ReclaimReceiver`(`build` が返す): 音声スレッドが手放した `Arc<SoundData>` を
  ゲームスレッド側で回収してドロップするためのキューの受信側。

この設計により、音声コールバック内部(`Mixer::render` 以下)は `Mutex` はおろか
`UnsafeCell` すら一切使わない、単一の書き手が `&mut self` で触るだけの通常の Rust コードで
書けている。

### PCM(`Arc<SoundData>`)所有権と回収経路

- ロード時、`SoundStorage`(ゲームスレッド専用)が `Arc<SoundData>` の「原本」を保持する。
- `mw_se_play` 相当のコマンド発行時に `Arc::clone`(参照カウント増加のみ、ヒープ確保無し)
  してコマンドへ載せ、音声スレッドの `VoicePool` へムーブする。
- ボイスが終了(自然終了・停止・スティール完了)した際、音声スレッドはその `Arc` を
  **その場でドロップしない**。`ReclaimSender`(`Mixer` 内部専用、`ReclaimReceiver` と対になる
  rtrb キューの送信側)へムーブで送り出す。ゲームスレッド側が `ReclaimReceiver::drain()` で
  回収してドロップする(= 実際のデアロケーションはゲームスレッド上で起こる)。
  **「コールバック内での Arc ドロップ禁止」がこの設計の核心。**
- 回収キューが満杯(通常運用では到達しない設計容量にしてある)の場合の最終手段として
  `std::mem::forget` で意図的にリークする(デアロケーションよりはリークの方が
  音声スレッドの安全性を保てるため。【仮】、M2 で発生頻度を計測し容量を見直す)。

## リアルタイム安全性規約(初期構築仕様 §5.3, 確定)

音声スレッド(オーディオコールバックから呼ばれる経路 = `Renderer::render`/`Mixer::render`
およびそこから呼ばれるすべてのコード)で禁止するもの:

- ロック取得(mutex / rwlock)、チャネルのブロッキング受信
- ヒープアロケーション / デアロケーション(`Vec::push` の暗黙の再確保、`Arc` のドロップを含む)
- ファイル / ネットワーク IO、システムコール一般、`println!` 系
- パニック経路(`unwrap` / `expect` / 添字パニック)。呼び出し元(mw-ffi)は panic 境界で
  保護するが、mw-core 自身もパニックしない実装を徹底する(添字は `get`/`checked_*` 系を使う)

この規約は `Renderer::render`/`Mixer::render` に限らず、ここに実装するミキサ・
ボイス管理・リングバッファ読み出しすべてに適用される。`VoicePool`・`BusSet`・`Ramp`・
`SoftClipper` は全てコンストラクタでのみ固定長バッファを確保し、以後の呼び出しでは
一切アロケーションしない。

### 検証手段(実装済み)

`tests/realtime_safety.rs` に `#[global_allocator]` としてカウンティングアロケータを
仕込んだ統合テストがある。`Renderer::render` 呼び出しの前後だけ計測フラグを立て、
ボイス満杯からのスティール・停止・バスフェード・クリッパ動作・予約 SE の挿入と発火
(`ScheduleQueue::try_insert`/`pop_front`、M2-5)を含むシナリオを200回コールバック相当分
レンダリングしてもアロケーション/デアロケーションが0回であることを固定化している。
M4-3 で BGM ボイスの再生 + ループ折り返しもこの同じシナリオへ加えた(BGM の
チャンクレンダリング経路〔`BGM_CHUNK_FRAMES` 固定長スタック配列〕も対象に含める
ため)。

⚠️ **CI(ubuntu-24.04)で不定期に落ちるフレークが M4-3 の push で発生**(2026-08-29,
`left: 4 right: 0` のような小さな非ゼロ値での失敗。ローカル環境では一度も再現しなかった)。
真因は計測フラグ・カウンタが**プロセス全体で共有される `AtomicBool`/`AtomicUsize`**
だったこと——計測ウィンドウ中に別スレッド(テストランナー自身のスレッドプール等)が
行った確保まで無差別に数えてしまっていた。対策として計測フラグを
**thread-local**(`RenderTrackingGuard` が RAII で立て下げする)にし、
「レンダー呼び出しを行っているスレッド自身の確保・解放だけ」を数える形にした
(カウンタ自体はプロセス全体の `static` のままだが、計測開始直前/直後の値の
差分〔delta〕を見ることで、複数の `#[test]` が並行しても正しく動く)。
この修正により、`#[test]` を複数持てないという制約は解消された——実際に
`render_tracking_is_isolated_from_concurrent_background_allocation` という2本目の
`#[test]`(バックグラウンドスレッドが確保し続けていてもレンダー経路の計測が0のまま
であることを固定化する回帰テスト)を追加してある。経緯の詳細は `docs/history.md`
「CI で4カウント落ちたフレークの真因と対策」を参照。

## 依存

`rtrb`(SPSC ロックフリーリングバッファ。コマンドキューと回収キューの両方に使う。
当初 mw-ffi 境界に置く想定だったが、`Renderer::render` が `&mut self` の単一所有権を保つには
`Consumer`/`Producer` をミキサ自身が持つ設計の方が単純なため mw-core に置いた)。
`symphonia`(デコード。wav / ogg vorbis、`decode.rs`)・`rubato`(リサンプル、`resample.rs`)
を M2 で追加した(初期構築仕様 §7.1)。どちらもゲームスレッド(SE ロード時)・
デコードスレッド(`pump()` の内側)からのみ呼ばれ、音声コールバック経路には一切入らない
(§5.3 のリアルタイム安全性規約はこの2クレートの呼び出し経路には適用されない設計)。
