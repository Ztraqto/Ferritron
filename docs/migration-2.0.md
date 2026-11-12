# Ferritron 1.4.x から 2.0 へ

2.0 の破壊的変更は次の表のものだけです。パラメータのキー、プラグインの `id`、保存データの互換は壊していません（1.x のプロジェクトを開けます）。
作業中の版（2.0.0 alpha）の記録です。**まだ完成していない項目は末尾に書いています。**

| # | 変更 | あなたがすること |
|---|---|---|
| BC1 | オートメーションの既定が**制御周期**になった（32 サンプル） | 音のずれは最大 31 サンプル。浮動小数点パラメータは `smooth` でつなぐ。1.x と同じサンプル精度が要るなら `[process] automation = "sample"`、自分でイベントを読むなら `"events"` |
| BC2 | `Context` のフィールドがメソッドになった（`#[non_exhaustive]`） | 下の対応表のとおり機械的に置き換える |
| BC3 | C ABI が v2 になった（`FERRITRON_ABI_VERSION 2`） | C++ / C# のバインディングを使うプラグインだけが対象。`process` が `ferritron_process_data` を受け取る |
| BC4 | 組み込みバイパスが 10 ms のクロスフェードと、プラグインのレイテンシに合わせた遅延付きになった | 戻す設定はない。切り替えを即時にしたいときは `role = "bypass"` のパラメータを自分で書く |
| BC8 | **Windows の VST3 で、明示した GUID（`[vst3] class_id` / `controller_id` / `compatibility.id`）がホストに書いたとおり見えるようになった** | 1.4.0 までは Windows のホストが、先頭 3 つのフィールドのバイト順を入れ替えた文字列で表示していた。クラス ID が変わるので、**出荷済みで `class_id` を明示しているプラグインは、旧 ID を `compatibility = { id = "...", replaces = ["<旧 ID>"] }` に載せる**（`replaces` は旧版が Windows のホストに見せていた文字列。例: 書いた `3E6B2A90-4C17-4D61-9D5B-1F7A0C2E8B11` は旧版で `902A6B3E-174C-614D-9D5B-1F7A0C2E8B11` と見えた）。`plugin.id` から作る ID は変わらない |
| BC6 | 平滑化を指定した浮動小数点パラメータの `value()` は「区間の先頭の平滑化後の値」 | ホストが設定した値は `target()` |
| BC5 | GPU エディタの `gpu_view::Input` が `#[non_exhaustive]` になり、ポインタのイベントに `modifiers`、`Wheel` に `delta_x` が付いた | `match` に `_ => {}` を足す。構造体のバリアントはフィールド名で取り出し、`..` で残りを無視する |
| BC7 | `.frtx`（1.2 より前の拡張子）を読まなくなった | `cargo ferritron migrate-frtx <package>` で `.ferritroject` に名前を変える（ビルドが止まって案内する） |

### 1.x で保存したプロジェクトが開けることの確認

パラメータのキー・ID、`plugin.id` から作るクラス ID、保存データは 1.x と互換です。自分のプラグインでも確かめるには、
1.4.0 で保存した状態とパラメータの一覧をフィクスチャとして残し、`ferritron::testing::check_fixture::<MyPlugin>(json)` を
テストで呼びます（形式は `tests/fixtures/1.4.0/*.json`。ID・順序・クラス ID が変わっていたり、保存値が読めなかったりすると失敗します。
BC8 でクラス ID が変わったプラグインは、旧 ID が `compatibility.replaces` にあれば通ります）。

## `Context` の対応表（BC2）

| 1.x | 2.0 |
|---|---|
| `ctx.params` | `ctx.params()` |
| `ctx.io` | `ctx.io()`（`&mut AudioIo`） |
| `ctx.notes` | `ctx.notes()` |
| `ctx.midi` | `ctx.midi()` |
| `ctx.sample_rate` | `ctx.sample_rate()` |
| `ctx.quality` | `ctx.quality()` |
| `ctx.transport` | `ctx.transport()`（`&Transport`） |

次のスクリプトで機械的に置き換えられます（ソースが git で管理されている状態で実行し、差分を確認してください）。

```bash
sed -E -i 's/\bctx\.(params|io|notes|midi|sample_rate|quality|transport)\b([^(]|$)/ctx.\1()\2/g' src/**/*.rs
```

`ctx.io()` は `&mut` を借りるので、`ctx.params()` の値を先に変数へ取り出してから音声を触ると借用の衝突を避けられます。

## プロジェクトファイルの新しいキー

```toml
[process]
automation    = "control"   # "control"（既定）| "sample"（1.x と同じ）| "events"（分割しない）
control_block = 32          # 制御周期（サンプル）。1〜4096

[params]
gain   = { type = "float", range = [-60, 12], default = 0, unit = "dB", smooth = "20ms" }
cutoff = { type = "float", range = [20, 20000], scale = "log", smooth = { time = "10ms", kind = "log" } }
```

- `smooth` の `kind`: `"linear"`（既定。指定の時間で届く）、`"log"`（比で直線。周波数向け）、`"exp"`（一次の指数平滑。時間は時定数）。
- 区間ごとのサンプル値は `ctx.ramp(&ctx.params().gain, &mut buf)`（`buf` は `activate` で確保する）。
- 変わったパラメータだけ見たいときは `ctx.changed(&p)` / `ctx.any_changed()` / `ctx.changed_indices()`。
  平滑化したパラメータは、平滑化した値が前の区間から動いた区間でも true になる。
- `"events"` モードでは `ctx.param_events()` に offset 順の変更が入る。パラメータの値はブロックの先頭のもの。

## C ABI v2（BC3）

- `ferritron_plugin_api::process` の引数が `(void* instance, const ferritron_process_data* data)` になった。
- `ferritron_process_data` は先頭の `size` に呼び出し側の `sizeof` を入れる。後からフィールドが増えても古い呼び出し側が読める。
- 出力イベント用の `out_events` / `out_capacity` / `out_count` を用意した（プラグインが書き込む機能は未実装）。
- シムのイベント整列は `ferritron_events.h` の `ferritron_sort_events()`（安定、O(n log n)）を使う。
- `ferritron.abi.v1` の関数群は提供しない。シムとコアは同じ版でだけビルドする。

## 検証の範囲

- Windows で実行して確かめたもの: VST3（testhost、2000 点のオートメーション）、`cargo test`。
- 型確認のみ・未検証: macOS / iOS / AU / AUv3 / AAX、aarch64 の非正規化数対策。
- 未完成: 出力イベント（B5）、VST3 以外のシム側の整列の置き換え、`Setup.control_block`。
