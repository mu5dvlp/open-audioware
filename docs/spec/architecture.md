# アーキテクチャ

> **この文書は仕様の一部**(旧 `init.md` の §5)。
> 全体の索引と凡例は [`README.md`](README.md)。
>
> 🔴 **コードからは「初期構築仕様『§n 話題名』」の形で参照する**(ファイル名に依存しない)。
> **§番号は移管前と同じ**なので、既存の参照はそのまま有効。


## 5. アーキテクチャ

### 5.1 クレート構成 【仮】

cargo workspace で3クレートに分ける。

```
crates/
  mw-core/       … ミキサ、ボイス管理、クロック、デコード、リサンプル。OS 非依存・デバイス非依存。
                    オフラインレンダリングはここだけで完結する(テストの主戦場)
  mw-backend/    … 出力デバイス抽象(Backend trait)と cpal 実装。
                    将来の oboe / RemoteIO 直叩き実装もここに並べる
  mw-ffi/        … C ABI 境界。ハンドル管理、コマンド/イベントキュー、csbindgen の入力
```

- 依存の向きは `mw-ffi → mw-core / mw-backend`、`mw-backend → mw-core` のみ。
  mw-core は他クレートに依存しない(テンプレート仕様のレイヤ思想と同じ「内向き依存」)

### 5.2 スレッドモデル 【確定】

```
[ゲームスレッド (C#)]
   │  FFI 呼び出し(全て非ブロッキング。内部はコマンドをキューに積むだけ)
   ▼
[コマンドキュー (SPSC ロックフリー)]        [イベントキュー (SPSC)]
   │                                          ▲  RouteChanged / Underrun / MusicEnded / StreamError
   ▼                                          │
[音声スレッド(OS のオーディオコールバック)]──┘
   … コマンド消化 → デコード済みリングバッファからミックス → 出力。
     ロック・アロケーション・ブロッキング IO 禁止
   ▲
[デコードスレッド]
   … 楽曲のストリーミングデコード(Symphonia)。音声スレッドへはリングバッファで供給。
     シーク時はここで再位置決めして詰め直す
```

- SE はロード時に全デコードしてメモリ常駐(PCM)。楽曲のみストリーミングデコード
- ロード(デコード)はゲームスレッドをブロックしない。完了は状態問い合わせで検知

### 5.3 リアルタイム安全性の規約 【確定】

音声スレッド(オーディオコールバック)内で禁止するもの:

- ロック取得(mutex / rwlock)、チャネルのブロッキング受信
- ヒープアロケーション / デアロケーション(`Vec::push` の暗黙の再確保を含む)
- ファイル / ネットワーク IO、システムコール一般、`println!` 系
- パニック経路(`unwrap` / `expect` / 添字パニック)。コールバックは panic 境界で保護し、
  万一のパニックは無音出力 + `StreamError` イベントに落とす

検証手段: デバッグビルドでコールバック内アロケーションを検出するカスタムアロケータフックを
仕込む 【仮】。加えてオフラインレンダリングのテストでコールバック相当経路を常時実行する。

### 5.4 FFI 設計原則 【確定】

- 全関数スレッドセーフ・非ブロッキング・エラーコード返し(§4.8)
- 毎フレーム呼ぶ関数(クロック取得・イベントポーリング)は **GC アロケーションゼロ**
  (blittable 構造体 + 呼び出し側バッファ)
- C# バインディングは csbindgen で Rust から自動生成し、手書きの宣言ズレを構造的に排除する(M2)
- シンボルは `mw_` プレフィックスで統一

### 5.5 主要 API の概形 【仮】

```
mw_init(config) / mw_shutdown() / mw_abi_version()
  // config: 希望バッファ長、ボイス数、リングバッファ秒数 等。init/shutdown は冪等

mw_sound_load(bytes, len, mode, out_id)  // mode: SE(全デコード常駐) / Music(圧縮のまま保持)
mw_sound_release(id)

mw_se_play(id, bus, volume) -> voice     // 即時発音(次コールバック)
mw_se_schedule(id, bus, host_time_ns)    // サンプル精度の予約発音(メトロノーム用)
mw_voice_stop(voice) / mw_voice_set_volume(voice, vol)

mw_music_set(id)                         // ストリーミング準備(プリロール込み)
mw_music_state() -> state                // Loading / Ready / Playing / Paused
mw_music_play_scheduled(host_time_ns)
mw_music_pause() / mw_music_resume_at(frames)   // 巻き戻し付き再開(中断対応)
mw_music_seek(frames) / mw_music_stop()
mw_music_set_loop(begin_frames, end_frames)     // プレビュー区間ループ
mw_music_get_position(out snapshot)             // { song_frames, host_time_ns, rate, is_playing, generation }

mw_bus_set_volume(bus, vol) / mw_bus_fade(bus, target, ms)
mw_get_output_latency_ns(out ns)
mw_poll_events(buf, cap) -> count
```

---
