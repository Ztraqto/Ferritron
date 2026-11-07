# Ferritron

[English](README.en.md) | **日本語**

Rust で書かれた、シンプルで速く、拡張しやすいオーディオプラグインフレームワークです。

> **状況: 開発初期。** Ferritron では現在、**Windows 向けの VST3 / CLAP** として
> ステレオの**エフェクトとインストゥルメント**をビルドできます（Audio Unit を含む
> macOS 版は実装済みですが、Mac の実機ではまだ確認していません）。サンプル単位の
> オートメーション、状態の保存、組み込みのバイパス、多言語対応、**HTML/JS と GPU の
> エディタ**、**ホットリロード**（DAW を動かしたまま再ビルド）に対応しています。
> サンプルは Steinberg 公式の VST3 バリデータ（詳細テスト 537/537）と
> clap-validator（失敗 0）に合格しています。設計とロードマップは
> [docs/architecture.md](docs/architecture.md) を参照してください。

## プラグインの全体

プロジェクトファイル `gain.frtx`（中身は TOML。パッケージごとに1つ、`Cargo.toml` の隣に置きます）:

```toml
[plugin]
id     = "dev.ferritron.examples.gain"
name   = "Ferritron Gain"
vendor = "Ztraqto"

[params]
gain = { type = "float", range = [-60, 12], default = 0, unit = "dB" }
```

`src/lib.rs`:

```rust
use ferritron::prelude::*;

#[derive(Default)]
struct Gain;

#[ferritron::plugin]
impl Process for Gain {
    fn process(&mut self, ctx: &mut Context) {
        let gain = db_to_gain(ctx.params.gain.value());
        ctx.io.for_each_sample(|s| *s *= gain);
    }
}
```

`Cargo.toml` の `[lib]` に `crate-type = ["cdylib"]` が必要です。パラメータは
プロジェクトファイルで一度だけ宣言します。`#[ferritron::plugin]` がそこから、型付きの
`ctx.params` 構造体、変わらないパラメータ ID、各形式のエントリポイントを生成します。
[examples/gain](examples/gain) を参照してください。

### エディタ（HTML / JS）

Web の UI をフォルダに置いて宣言します。UI はプラグインに埋め込まれ、システムの
WebView（Windows では WebView2）で表示されます。ローカルサーバーは不要で、VST3 と
CLAP のどちらでも動きます。

```toml
# gain.frtx
[view]
backend   = "web"
entry     = "ui"          # must contain index.html
size      = [480, 240]    # logical pixels at 100% scale
resizable = true          # optional; then min_size / max_size (defaults: half size / 4096)
```

```toml
# Cargo.toml
ferritron = { ..., features = ["web"] }
```

```js
// ui/app.js: the page talks to the plugin through window.ferritron
const p = ferritron.params.gain;      // { name, unit, min, max, value, text, ... } (localized)
slider.addEventListener("pointerdown", () => ferritron.begin("gain"));
slider.addEventListener("input", () => ferritron.set("gain", slider.value));   // plain value (dB)
slider.addEventListener("pointerup", () => ferritron.end("gain"));
ferritron.onChange((key, param) => show(param));   // automation, host or UI changes
title.textContent = ferritron.t("ui-title");       // from locales/<lang>/*.ftl
```

エディタの文言は、パラメータと同じ翻訳ファイルを使います。`ferritron.t(key, args)`
は任意のメッセージをユーザーの言語で返し、`{ $name }` の変数は `args` から埋められます。
`cargo ferritron i18n check` は、英語にはあるのに他の言語にないエディタの文言も報告します。

編集はジェスチャーとしてホストに伝わるので、DAW はオートメーションとして記録します。
[examples/gain/ui](examples/gain/ui) を参照してください。

- **すぐ開く**: Windows では、エディタの裏で動くブラウザ（WebView2）を、最初のプラグイン
  インスタンスが作られた少し後に、ホストを止めずに起動し、インスタンスがある間は動かし
  続けます。そのためエディタは 300 ms 以上ではなく約 60〜80 ms で開きます
  （`ferritron-testhost` で計測）。プロセス内のすべてのエディタがこれを共有します。
  `[view]` に `preload = false` と書くと、エディタを開いたときに初めて起動し、最後の
  エディタとともに終了します。
- **白いちらつきなし**: `[view]` に `background = "#16181d"` と書くと、ページが描画
  されるまで最初のフレームからこの色で塗ります。ページの背景色を指定してください。
- **右クリック**: ブラウザのメニュー（再読み込み、検証など）は出ません。ページには
  `contextmenu` イベントが届くので、エディタ独自のメニューを出せます。
  `ferritron.nativeContextMenu = true` でブラウザのメニューに戻せます（開発中など）。
  ブラウザのキー（F5、Ctrl+P、Ctrl+F）も無効です。
- **組み込みスイッチ**: `ferritron.builtins` には `"ferritron.bypass"` と、`[quality]`
  がある場合は `"ferritron.quality"` が、`ferritron.params` と同じ形で入っています。
  `ferritron.set` / `edit` / `onChange` はこれらのキーでも動きます。

- **HiDPI**: サイズは論理ピクセルで、エディタはホストの拡大率（VST3
  `IPlugViewContentScaleSupport`、CLAP `set_scale`）、なければウィンドウの DPI に合わせて
  拡大されます。ページは論理サイズでレイアウトされ、くっきり描画されます。
- **サイズ変更**: `resizable = true` にすると、ホストは制限の範囲でエディタのサイズを
  変えられます。ページは相対的な CSS 単位でレイアウトしてください。ユーザーが選んだ
  サイズはプロジェクトに保存され（拡張 `ferritron.editor-size.v1`）、プロジェクトを
  開き直した後も、エディタはそのサイズで開きます。

DAW なしでエディタを確認するには
`ferritron-testhost <bundle> --gui <screenshot dir> [--click X Y] [--scale 1.5] [--resize W H]`
を使います。`timing:` の行（読み込み、インスタンス、エディタの表示）を出力します。
`FERRITRON_TESTHOST_OPEN_AFTER=S` でインスタンス作成の S 秒後に（ユーザーと同じように）
エディタを開き、`FERRITRON_TESTHOST_REOPEN=N` で N 回閉じて開き直します。
`FERRITRON_TRACE=1` を設定すると、プラグインが時間の内訳を出力します。クリックは実際の
マウスを動かします。`FERRITRON_TESTHOST_POST_CLICK=1` にすると、その位置のウィンドウに
メッセージとして送ります（Web エディタは入力が WebView2 を通るので対象外です）。
`--resize` の後は、2つ目のインスタンスが保存された状態を読み込み、エディタが開き直す
サイズを報告します。

### 別プロセスのエディタ

どのエディタ（Web でも GPU でも）も、専用のプロセスで動かしたまま、通常どおりホストの
ウィンドウに埋め込めます:

```toml
[view]
process = true
```

パラメータの値とエディタでの編集は共有メモリでやり取りし、編集は引き続きジェスチャー
としてホストに届きます。エディタのプロセスがクラッシュしても、ホストと音には影響せず、
1秒ほどで新しいエディタのプロセスが代わりに立ち上がります。独自のイベントループを
必要とする UI フレームワークにも向いています。エディタのプロセスは、`rundll32` で
起動したプラグインのライブラリそのものです（追加で配布するものはありません）。
`[isolation] mode = "process"` のプラグインは、常にこの方法でエディタを開きます。
Windows のみです。macOS では他のプロセスのウィンドウを埋め込めないため、エディタは
ホストのプロセスで開きます（分離したプラグインも同様で、そのエディタは同じミラーを
通じて分離されたインスタンスとやり取りします）。

### GPU で描くエディタ（wgpu）

すべてを自分で描くエディタ（独自のノブ、スペクトラム表示、3D など）には、
`backend = "gpu"` で、Web ページの代わりにホストのウィンドウ内の wgpu サーフェスを
プラグインに渡します。描画する型はプロジェクトファイルで指定します:

```toml
[view]
backend = "gpu"
editor  = "Editor"        # a type implementing ferritron::gpu_view::GpuEditor
size    = [480, 240]
resizable = true
```

```rust
use ferritron::gpu_view::{Frame, Gpu, GpuEditor, Input, Ui};

impl GpuEditor for Editor {
    fn new(gpu: &Gpu, ui: &Ui) -> Self { /* pipelines for gpu.format */ }
    fn render(&mut self, gpu: &Gpu, frame: &Frame, ui: &Ui) {
        let gain = ui.normalized("gain");   // also ui.value / ui.text (localized) / ui.t
        /* draw into frame.target, submit to gpu.queue */
    }
    fn input(&mut self, event: &Input, ui: &Ui) -> bool {
        if let Input::Press { x, .. } = *event { ui.edit_normalized("gain", x / ui.width()); }
        true   // redraw
    }
}
```

`ferritron = { ..., features = ["gpu-view"] }` で使えます。エディタは、パラメータが
変わったとき（オートメーション、ホスト、エディタ）、再描画を求める入力の後、サイズ変更時、
そして `animating()` が true を返す間は毎ティック（毎秒約60回）再描画されます。編集は
ジェスチャーとしてホストに届きます。入力は論理ピクセルで届き、`frame` は物理サイズと
拡大率を持ちます。プロセス内のすべてのエディタが1つの GPU デバイスを共有し、表示は
垂直同期を待たないので、ホストの UI スレッドを止めません。文字の描画はプラグインに
任せています（wgpu 上のグリフライブラリなど）。

キーボード: クリックするとエディタにキーボードフォーカスが移ります。
`wants_key(key, modifiers)` でエディタが使うキーを指定すると、そのキーは `Input::Key`
（押下、離す、リピート）として、入力された文字は `Input::Char`（キーボード配列と IME の
処理後）として届き、`Input::Focus` も届きます。それ以外のキーはホストのウィンドウに
渡るので、エディタにフォーカスがあっても DAW のショートカット（スペースで再生など）は
そのまま使えます。別プロセスのエディタ（`[view] process = true`）でも同じです。
[examples/gpu-view](examples/gpu-view)（1つのシェーダーで描くレベルバー）を参照してください。

### 無音時の停止と書き出し時の音質

**待機中のプラグインは負荷ゼロ。** プラグインの入力が無音（-120 dBFS 未満）のまま、
出力が続きうる時間が過ぎると、Ferritron は `process` の呼び出しをやめて無音を出力します。
入力、ノート、パラメータの変化があれば同じブロックで再開します。ホストにも伝えるので
（VST3 の無音フラグとテール長、CLAP の `CLAP_PROCESS_SLEEP` と `tail` 拡張）、ホスト側も
処理を省けます。出力が続く時間は `Process::tail` で指定します:

```rust
fn tail(&self) -> Tail {
    Tail::Samples((self.decay_seconds * self.rate) as u32)   // a reverb
}
```

- `Tail::Auto`（既定）: 入力と出力がどちらも無音で、ノートも押されていない状態が2秒
  続いたら停止します。ほとんどのエフェクトとインストゥルメントはこれで十分です。
- `Tail::Samples(n)`: 入力が止まってから `n` サンプルで出力が消えます（レイテンシは
  加算されます）。エコーの間隔が2秒を超えうるディレイなどに使います。まだ鳴っている
  音を切ることはありません。
- `Tail::Infinite`: 停止しません（ジェネレーター、テストトーンなど）。

**書き出し時は自動で最高音質。** `ctx.quality` は、ホストがオフラインでレンダリング
している間（書き出し、バウンス、フリーズ）は `Quality::Best`、ユーザーが再生や編集を
している間は `Quality::Normal` です。ホストが指示したブロックからすぐ切り替わります。
各レベルで何をするかはプラグインが決めます:

```rust
let taps = if ctx.quality.is_best() { 512 } else { 64 };   // oversampling, filter length, ...
```

プロジェクトファイルに `[quality]` を書くと、ユーザーが手動で選べる「音質」スイッチ
（自動 / 標準 / 最高）も付きます。「自動」は上記のルールです。プロジェクトに保存され、
オートメーションはできません。`Setup::quality` で `activate` 時の値がわかるので、
レベルごとに必要なバッファが違う場合は、そこで `Quality::Best` 用に確保してください。

```toml
[quality]
```

テストでは、`Harness::set_offline(true)` で書き出しを開始し、`Harness::sleeping()` で
直前のブロックが停止していたかを確認できます。

### 複数コアの利用

`ctx.parallel_for` は、ボイスなどの独立した処理を複数のコアに分散します:

```rust
ctx.parallel_for(&mut self.voices, |voice| voice.render(frames));   // then mix serially
```

形式がスレッドプールを提供していれば（CLAP の `thread-pool`）ホストのものを使います。
なければ、`[plugin]` に `parallel = true` がある場合に限り、MMCSS の「Pro Audio」優先度に
上げた Ferritron のワーカーを使います。どちらもなければ呼び出したスレッドで実行します。
どの場合も出力は同じです。メモリ確保やブロックは一切せず、別のインスタンスがワーカーを
使っていれば、待たずに呼び出したスレッドで実行します。`parallel = true` がなければ
スレッドは起動しません。

プロセス内のすべての Ferritron プラグインは、DLL や開発元が違っても、**1組**の
ワーカーを共有します。プラグインがそれぞれスレッドを持ち込んで同じコアを奪い合うことを
避けるためです。`ferritron-testhost --shared-runtime A.dll B.dll` で確認できます。

`cargo run --release -p ferritron-bench` で計測できます。16スレッドの PC で、重い32
ボイスが 256 サンプルのブロックあたり 3106 µs から 449 µs（6.9倍）になり、出力は直列
実行と完全に一致しました。

### GPU 処理

`ferritron-gpu` は重い DSP を GPU（wgpu: Vulkan / DX12 / Metal）で動かします。最初の
部品は、レイテンシで分割する長い畳み込み `Convolver` です。最初のタップ（2〜3ブロック分）
は CPU で畳み込むので、レイテンシはありません。長いテールは GPU で、少なくとも1ブロック
先行して分割 FFT 畳み込みで処理するので、IR が長くなってもコストはゆっくりしか増えません。
ホストは毎ブロックでブロックサイズを変えてもかまいません。オーディオスレッドは GPU API を
呼ばず、1つの GPU デバイスをプロセス全体で共有します。

```rust
// in activate():
self.reverb = ferritron_gpu::Convolver::new(&impulse_response, setup.max_block_size)?;
// in process():
self.reverb.process(ctx.io.channel_mut(0));
```

2秒の畳み込みリバーブ [examples/gpu-reverb](examples/gpu-reverb) を参照してください。
RTX 3070、256 サンプルのブロックでの結果（`cargo run --release -p ferritron-bench`）:

| インパルス応答 | CPU で直接畳み込み（1コア） | Ferritron のオーディオスレッド | GPU（並行） | 期限超過 |
|---|---|---|---|---|
| 2 秒（96,000 タップ） | 21.1 ms（予算の 396%） | 115 µs（2.2%） | 256 µs | 400 中 0 |
| 10 秒（480,000 タップ） | 108.7 ms（2037%） | 115 µs（2.2%） | 313 µs | 400 中 0 |

CPU の比較対象は直接畳み込みで、FFT を使う CPU 実装ではありません。GPU の時間の大半は、
ブロックごとに1回の送信と読み戻しにかかる固定コストです。GPU があるブロックに間に
合わなければ、そのブロックは先頭部分だけになり、`missed_blocks()` が数えます。

#### IIR フィルタバンク

IIR フィルタは直列に見えますが、状態は線形漸化式に従うので、`IirBank` はブロックを
並列スキャンで計算します。多数のフィルタチェーン（バイクアッドのレーン）を同時に動かし、
出力先が同じレーンは GPU 上で合算します。モーダル合成、フィルタバンク、多チャンネルの
EQ に使えます。各セクションは数値的に安定な形で実現しているので、結果は CPU 上の
素朴な f32 バイクアッドと同等以上の精度です。係数の変更は転置直接形 II とまったく同じ
ように振る舞います。

```rust
use ferritron_gpu::{Biquad, Lane, RealtimeIirBank};
let lanes: Vec<Lane> = modes.iter()
    .map(|&f| Lane { input: 0, output: 0, sections: vec![Biquad::bandpass(f, 300.0, rate)] })
    .collect();
let mut bank = RealtimeIirBank::new(&lanes, max_block)?; // in activate()
bank.process(&[input], &mut [output]);                   // in process(): never waits
```

IIR は畳み込みのようにレイテンシで分割できません。そこで `RealtimeIirBank` は出力を
`latency()` フレーム（2 × `max_block`）遅らせ、GPU の処理を専用のスレッドで動かします。
それでも GPU が遅れた場合は、CPU 上のバンクのコピーが、GPU が最後のフレームとともに
返した状態から欠けたフレームを計算するので、出力は変わりません。GPU が完全に失敗しても
CPU が処理を続けます。`IirNode` は、CPU が1ブロックを処理できる（予算の半分以内）ときに
これを有効にします。

プロセス内のすべてのリアルタイムバンクは1つの GPU スレッドを共有し、全インスタンスの
待ちブロックを1回の送信にまとめます（オフライン用には `IirBank::process_many` が同じ
ことをします）。同じプラグインを32トラックで、各256フィルタ: インスタンスごとに送信すると
ブロックあたり 5,151 µs（予算の 97%）、まとめると 1,695 µs。リアルタイムでは32個すべてで
ブロックあたり GPU 時間 484 µs、期限超過なしでした。`IirBank` は GPU を待つので、
オフラインレンダリングに向いています。2,000 個のレゾネーターを合算した場合
（`ferritron-bench`）:

| | CPU（1コア） | Ferritron |
|---|---|---|
| リアルタイム、256 サンプルのブロック | ブロックあたり 1,795 µs（予算の 34%） | オーディオスレッドで 6.6 µs、GPU は並行して 270 µs、400 中 0 超過 |
| 10 秒のオフラインレンダリング | 3.36 秒 | 0.061 秒（55倍速） |

モーダルシンセサイザー [examples/gpu-modal](examples/gpu-modal) を参照してください。
16ボイス、各128レゾネーター（合計 2,048）を、ノイズのマレットで励起します。ノートを
離すと、係数の変更をまたいで状態が保たれるので、クリックなしで減衰が短くなります。
GPU がなければ同じフィルタが CPU で動きます。

#### レイテンシの報告

出力を遅らせるプラグインはそれを報告し、ホストが補正します:

```rust
impl Process for MySynth {
    fn latency(&self) -> u32 {
        self.bank.latency() as u32 // read after every activate()
    }
    // ...
}
```

VST3（`getLatencySamples`）と CLAP（`latency` 拡張）がホストに伝えます。

#### オフラインレンダリング

ホストがバウンスや書き出しをするとき、`process` は返った直後にまた呼ばれます。すると
リアルタイム用の GPU 部品は、待てない処理を捨ててしまいます。そこで `Setup::offline` が
VST3 の `processMode` や CLAP の `render` 拡張からそれをプラグインに伝え、GPU 部品は
代わりに待つことができます:

```rust
fn activate(&mut self, setup: &Setup) {
    let mode = ferritron_gpu::Mode::from_offline(setup.offline);
    self.reverb = ferritron_gpu::Convolver::with_mode(&ir, setup.max_block_size, mode).ok();
    // IIR banks: IirBank (waits, no latency) offline, RealtimeIirBank otherwise.
}
```

2秒のリバーブを通した60秒のバウンス（`ferritron-bench`）: リアルタイムモードは 1.25 秒で
終わりますが、11,250 ブロック中 44 でテールが欠けます。オフラインモードは 2.09 秒
（実時間の29倍速）で、何も欠けません。どちらのサンプルもこの方法でモードを切り替えます。
テストでは `Harness::offline` でレンダリングします。

#### ノードグラフと自動配置

`ferritron-graph`（`ferritron_gpu::graph` として再エクスポート）は、ノードをチャンネル
単位でつなぎ、毎ブロック順に実行します。同じ入力に入る信号は合算されます。ノードが
レイテンシ（GPU パイプライン、先読み）を加えると、グラフは他の経路を遅らせて揃え、
合計を `graph.latency()` として報告します。

```rust
use ferritron_gpu::graph::{Gain, Graph, NodeSetup, Policy};
let mut graph = Graph::new(1, 1);
let conv = graph.add(ferritron_gpu::ConvolverNode::new(&ir, &setup)?);
let wet = graph.add(Gain::new(1, 0.3));
graph.connect(Graph::INPUT, 0, conv, 0)?;
graph.connect(conv, 0, wet, 0)?;
graph.connect(wet, 0, Graph::OUTPUT, 0)?;
graph.connect(Graph::INPUT, 0, Graph::OUTPUT, 0)?; // dry, summed
graph.prepare(max_block)?;
graph.process(&[input], &mut [output]);            // audio thread
graph.node_mut::<Gain>(wet).unwrap().set(0.5);     // change a node
```

ノードの配置は計測で決まります。`IirNode` は CPU バンクと GPU バンクを作って両方を
動かし、GPU ポリシーが許す範囲で速いほうを残します:

| ポリシー（`FERRITRON_GPU_POLICY` でプラグインの指定を上書き） | 許可 |
|---|---|
| `off` | CPU のみ |
| `balanced`（既定） | レイテンシが増えない場合のみ GPU |
| `max` | CPU がブロックの 1/8 を超える場合、レイテンシが増えても GPU |

256 サンプルのブロック（`ferritron-bench`）では、リアルタイムの `max` は 256 フィルタ
までを CPU に残し、1,024 以上を GPU に移します。オフラインでは 256 フィルタから GPU が
勝ちます。`examples/gpu-reverb` はグラフとして作られ、`examples/gpu-modal` は `IirNode`
を使っています。

#### GPU ステージの融合

GPU 部品はどれもブロックごとに固定コスト（アップロード、送信、待機、読み戻し）を払います。
直列のバンクは代わりに1つのチェーン `IirBank::chain(&[&a, &b, &c], max_block)` に
できます（`RealtimeIirBank::chain`、`CpuIirBank::chain`、`IirNode::chain` も）。ステージ
`k` はステージ `k - 1` の出力を読み、信号は GPU 上に留まり、チェーン全体で往復は1回です:

| 1,024 レゾネーター → 16 チャンネル EQ → ステレオ、256 サンプルのブロック | 別々のバンク | 融合したチェーン |
|---|---|---|
| ブロックあたりの GPU 時間（オフライン） | 559 µs（往復3回） | 224 µs（往復1回） |
| リアルタイムのレイテンシ | 1,536 サンプル | 512 サンプル |

グラフはこれを自動で行います。あるノードがチャンネルごとに次のノードだけに出力している
場合、`prepare()` が2つに融合を求めます（`Node::fuse`）。直列の IIR ノードは1つのチェーンに
なり（全体として配置し直されます）、直列のゲインは1回の乗算になります。融合されたノードの
ID はそのまま使えます: `graph.node_mut::<IirNode>(eq).set_lane(2, ...)` は、そのノードの
レーン 2 に届きます。`graph.set_fusion(false)` で無効にできます。重い IIR ノード（各 1,024
フィルタ）を3つ直列にすると、融合なしはブロックあたり 725 µs・レイテンシ 1,536 サンプル、
融合ありは 315 µs・512 サンプルでした。

#### Slang カーネル: 一度書けば CPU でも GPU でも動く

カーネルは1つの Slang 関数で、各チャンネルの各サンプルに対して呼ばれます:

```slang
// kernels/drive.slang
float process(float x, KernelArgs args)
{
    float drive = max(args.values[0], 1e-6);
    return tanh(x * drive) / tanh(drive) * args.values[1];
}
```

`ferritron-slang` はこれをビルドスクリプトで、GPU 用に WGSL へ、CPU 用に C++ へ
コンパイルします（C++ はコンパイルしてプラグインにリンクされます）:

```rust
// build.rs
fn main() {
    ferritron_slang::compile("drive", "kernels/drive.slang").unwrap_or_else(|e| panic!("{e}"));
}
// lib.rs
let node = ferritron_gpu::KernelNode::new(ferritron_gpu::slang_kernel!(drive), 2, &setup);
```

`KernelNode` は両方を計測して速いほうを動かします（GPU はオフラインレンダリングで提案
されます）。プラグインの SDK と同様、Slang は同梱していません。
https://github.com/shader-slang/slang/releases からリリースをダウンロードし、
`SLANG_DIR` を設定してください。[examples/slang-drive](examples/slang-drive) を参照して
ください。

#### Ferritron Chain: 1つのグラフにモジュールを並べる

DAW 内のプラグインは CPU 上で音声を受け渡すので、GPU プラグインはそれぞれ往復のコストを
払います。1つのプラグインに組み込んだモジュールは、代わりに1つのグラフを共有します。
`Module` がノードを追加し、`Graph::chain` がモジュールを直列につなぎ、融合は境界を越えて
働きます。[examples/gpu-chain](examples/gpu-chain)（共鳴ボディ 1,024 フィルタ、トーン EQ、
レベル）を参照してください。ボディとトーンは1つの GPU チェーンとして動きます。

#### オフラインのバッチ、状態を持つカーネル、多数のインスタンス

- `ferritron_graph::Batched::new(node, 8192)` は、ホストのブロックサイズに関係なく
  8192 フレームのバッチでノードを動かし、バッチをレイテンシとして報告します。オフライン
  レンダリングではバッチごとに GPU 往復1回です（4000 レゾネーターで、256 のホスト
  ブロックより 4.8 倍速）。
- Slang カーネルは、ブロックをまたいでチャンネルごとの状態を持てます:
  `float process(float x, KernelArgs args, inout KernelState state)` を
  `slang_kernel!(name, stateful)` で結び付けます。直列のカーネルノードは1つのチェーンに
  融合されます（ブロックあたり往復1回）。
- プロセス内のすべての畳み込みは1つの GPU スレッドを共有し、全インスタンスの期限の来た
  処理をまとめて送信します。

#### パーシステントカーネル、Tensor コア、RT コア

`ferritron-vulkan` クレートは、wgpu ではできないことを、wgpu と同じ Vulkan デバイス上で
追加します（2つ目のキューを使うので、両方が並行して動きます）。シェーダーは Slang で
書かれていますが、SPIR-V をリポジトリに置いてあるので、ビルドに Slang は不要です。

```rust
// A persistent kernel: the GPU in real time at 64-sample blocks, no latency.
let modes = IirNode::new_low_latency(&lanes, &setup);       // opt-in, see below
// Tensor cores: hundreds of channels mixed to hundreds of channels.
let mut mixer = MatrixMixerNode::new(256, 256, &setup);      // CPU or Tensor cores, measured
mixer.set_gain(out, input, 0.5);
// RT cores: a room's impulse response from its geometry.
let mut room = ferritron_gpu::room::Room::shoebox([10.0, 8.0, 4.0], 0.15);
room.add_box([2.0, 2.0, 0.0], [3.0, 3.0, 1.0], 0.3);
let (energy, tracer) = ferritron_gpu::room::energy_response(&room, &trace);
let ir = ferritron_gpu::room::impulse_response(&energy, 64, 1);
```

- **パーシステントカーネル**（`IirNode::new_low_latency`）: カーネルが GPU 上に常駐し、
  各ブロックをメモリ経由で受け取ります。RTX 3070 で 64 サンプル × 2048 レゾネーターが
  46 µs（中央値、99パーセンタイルで 49 µs）。音声の動作中は GPU の一部を待機に使い続け、
  コンピューター上の他の GPU 処理を遅くするので、明示的に有効にする必要があり、
  コンピューターごとに1つです。
- **Tensor コア**（`MatrixMixerNode`、`TensorMixer`）: 協調行列を使い、積は f16、和は
  f32（有効数字約3桁）です。256×256 チャンネル × 512 フレームで、GPU は 0.35 ms、CPU は
  4 ms。
- **RT コア**（`room`）: 部屋の加速構造に対するレイクエリ、または CPU 上の同じアルゴリズム。
  家具のある 1,116 三角形の部屋で、GPU は 2 ms、CPU は 5.8 秒。残響時間はセイビンの式と
  一致します。

### C++ でプラグインを書く

`ferritron.hpp`（C++20、ヘッダのみ、`ferritron_abi.h` の隣）は、クラスを Ferritron
プラグインにします。各形式のアダプタは他のプラグインと同じように扱います:

```cpp
#include "ferritron.hpp"

struct Gain {
    static constexpr ferritron::Info info{"com.example.gain", "Gain", "Example", "1.0.0"};
    // key, name, unit, min, max, default, steps (0 = continuous)
    static constexpr ferritron::Param params[]{{"gain", "Gain", "dB", -60.0, 12.0, 0.0}};

    void process(ferritron::Block& block) {
        float g = ferritron::db_to_gain(block.param(0));
        for (auto channel : block.channels())
            for (float& s : channel) s *= g;
    }
};
FERRITRON_PLUGIN(Gain)
```

Rust のプラグインと同様に、パラメータの変更は正確なサンプルで反映され、バイパス
パラメータが追加され、状態が保存され、デノーマルは切り捨てられ、例外が ABI を越える
ことはありません。パラメータ ID とクラス ID は Rust と同じ方法で計算されます。小さな
Cargo クレートが C++ をコンパイルし、形式のアダプタをリンクします:
[examples/cpp-gain](examples/cpp-gain) を参照してください。（MSVC 14.50 はパラメータ表の
指示付き初期化子でクラッシュするので、位置による初期化を使ってください。）

### C# でプラグインを書く

`bindings/csharp/Ferritron.cs` は、C# のクラスを Ferritron プラグインにします。NativeAOT で
ネイティブライブラリとしてビルドされます（ユーザーに .NET のインストールは不要です）:

```csharp
public sealed class Gain : Plugin
{
    static readonly PluginInfo Info = new("com.example.gain", "Gain", "Example");
    static readonly Param[] Params = { new("gain", "Gain", "dB", -60, 12, 0) };

    public override void Process(in Block block)
    {
        float g = Dsp.DbToGain(block.Param(0));
        for (int c = 0; c < block.Channels; c++)
            foreach (ref float s in block.Channel(c)) s *= g;
    }

    [ModuleInitializer]
    internal static void Register() => Host.Register<Gain>(Info, Params);
}
```

このライブラリは*コンパニオン*です。プラグイン本体は、形式のアダプタと
`ferritron_core::companion_plugin!("Gain.dll")` を持つ小さな Rust クレートで、プラグインの
フォルダからライブラリを読み込みます。`cargo ferritron bundle` がそこへコピーします
（プロジェクトファイルの `[companion] files`）。`Process` ではメモリを確保しないでください
（オーディオスレッドでガベージコレクタを動かさないため）。
[examples/csharp-gain](examples/csharp-gain) を参照してください。.NET SDK（8 以降）と、
Windows で NativeAOT を使うには Visual Studio の C++ ツールが必要です。

独自のランタイムを持つ他の言語（Python など）も、`ferritron_plugin_entry` をエクスポート
するコンパニオンライブラリとして同じ方法で組み込めます。

### ARA 2（Audio Random Access）

ARA ホスト（Studio One、Cubase、Logic、REAPER など）は、オーディオファイル全体を
プラグインに渡し、そのリージョンがどこで再生されるかを伝えます。そのためプラグインは
ファイルのどの部分にもいつでもアクセスできます。ピッチ・タイム編集、修復、解析の
ツールに必要な機能です。

```toml
[plugin]
url = "https://example.com/my-plugin"   # required by ARA

[ara]
```

```rust
use ferritron::ara::{AraRender, Source};

impl AraRender for MyPlugin {
    // Called for each part of a playback region inside the block.
    fn render_region(&mut self, params: &Params, source: &Source, start: i64, out: &mut [&mut [f32]]) {
        source.copy_to(start, out);   // or anything: source.channel(c) is the whole file
    }
}
```

`ferritron = { ..., features = ["ara"] }` と、`ARA_SDK_DIR` に置いたご自身の ARA SDK
（`git clone --recursive https://github.com/Celemony/ARA_SDK`、Apache 2.0。Ferritron には
同梱していません）で使えます。これにより、VST3 アダプタは「ARA Main Factory Class」と
`IPlugInEntryPoint2` を、CLAP アダプタは ARA のファクトリとプラグイン拡張を提供します。
Ferritron はホストが有効にした各オーディオソースをメモリに保持し、リージョンの音声は
通常の入力と同じようにプラグインの `process` を通ります。`[ara]` がなければ ARA 関連は
何もリンクされません。どちらのアダプタもモノラルのバスを受け付けます（ARA ホストは
モノラルのファイルをそのようにレンダリングします）。
[examples/ara-reverse](examples/ara-reverse) はファイルを逆再生します。

**タイムストレッチと解析。** ホストはリージョンを時間方向に伸縮できます（ピッチは
変わりません）。`render_stretched` には既定の実装があり、グレイン同士を揃える（波形の
類似度による）グラニュラーストレッチです。伸縮したタイムラインに追従できない
プラグイン（逆再生のサンプルなど）は `const TIME_STRETCH: bool = false` で無効にできます。
`const ANALYZES_NOTES: bool = true` を設定したプラグインは、各オーディオファイルのノート
（ピッチ、レベル、開始と長さ。ピッチエディタが表示するもの）をホストに渡します。
`analyze_notes(source)` にはモノフォニックのソース向けの既定の実装
（`ferritron::ara::detect_notes`、YIN）があり、バックグラウンドのスレッドで動きます。
伸縮はリージョン全体で線形です。テンポに追従する伸縮
（`kARAPlaybackTransformationTimestretchReflectingTempo`）には対応していません。
[examples/ara-stretch](examples/ara-stretch) は両方の既定の実装を使っています。
どちらのサンプルも、ARA SDK の `ARATestHost` のすべてのケースに VST3 と CLAP で合格します。

### クラッシュの隔離

既定では、プラグインのコードで起きたパニックは捕捉され、プラグインは無音を出力します。
本当のクラッシュ（アクセス違反、ネイティブライブラリ）から守るには、プラグインを専用の
プロセスで動かします:

```toml
[isolation]
mode = "process"      # default: "guarded" (in-process)
share = "plugin"      # default: instances of the plugin share one process; "instance": one each
pipeline = false      # true: blocks run one block late, in parallel with the host
```

プラグインのプロセスが落ちても、ホストは動き続け、プラグインは無音を出力します。新しい
プロセスが自動で起動し、同じパラメータで処理が再開します（通常は 0.5 秒以内）。子
プロセスは Windows の `rundll32` で起動した同じプラグインのライブラリなので、追加で
配布するものはありません。`FERRITRON_ISOLATION=process|guarded` で設定を上書きできます。
分離したプラグインのエディタは専用のプロセスで開く（「別プロセスのエディタ」を参照）
ので、エディタがクラッシュしても音は途切れません。

- **共有**: 既定では、1つのホスト内のプラグインのインスタンスはすべて1つのプロセスで
  動き（インスタンスごとに1スレッド）、メモリを節約します。その場合、クラッシュすると
  プロセスが再起動するまで全インスタンスが無音になります。`share = "instance"` で
  インスタンスごとに専用のプロセスになります。
- **パイプライン**: 既定では、ホストはブロックごとに処理を待ちます（レイテンシは増え
  ませんが、ブロックごとにプロセス間の往復が1回あります）。`pipeline = true` にすると、
  ホストが先に進む間に子プロセスがブロックを処理します。出力は1ブロック（ホストの最大
  ブロックサイズ）遅れ、レイテンシとしてホストに報告されます。
- 子プロセス内のプラグインが報告する**レイテンシ**はホストに届きます（パイプラインの
  ブロック分が加算されます）。
- **ホットリロード**は分離したプロセス内でも動きます。開発用バンドルは、再ビルドした
  コードをそこで読み込み直します。
- `FERRITRON_ISOLATION_SHARE=plugin|instance` と `FERRITRON_ISOLATION_PIPELINE=0|1` で
  プロジェクトファイルの設定を上書きできます。

Windows と macOS に対応しています。Windows では子プロセスは `rundll32` で動くプラグインの
ライブラリで、macOS では小さなヘルパー `ferritron-host` です。`cargo ferritron bundle` が
これを各バンドルの `Contents/Helpers` にコンパイルします（そこではシグナルは名前付き
FIFO を通ります）。サンドボックス化されたホストはこれを起動できないので、分離した
Audio Unit はサンドボックス非対応と申告します。
[tools/crash-fixture](tools/crash-fixture) は実際にクラッシュするプラグインで、試すのに
使えます:

```bash
ferritron-testhost "target/bundled/Ferritron Crash Fixture.vst3" --seconds 3 --pulse-param-at 1
```

#### WebAssembly サンドボックス（分離レベル 2）

`wasm32-wasip1` 向けにビルドしたプラグインは、ホスト内の wasmtime サンドボックスで
動かせます。`ferritron-wasm` がゲストのモジュールの上に C ABI を実装します。ゲストは
ファイル、環境変数、ネットワークにアクセスできず、内部でのクラッシュや 500 ms を超える
呼び出しは、そのプラグインを無音にするだけです。プラグイン本体はモジュールを埋め込む
小さなクレートです:

```rust
ferritron_wasm::wasm_plugin!(include_bytes!(concat!(env!("OUT_DIR"), "/guest.wasm")));
```

[examples/wasm-sandbox](examples/wasm-sandbox) を参照してください。
[examples/wasm-crusher](examples/wasm-crusher)（試すための `crash` と `hang` スイッチ付きの
ビットクラッシャー）を動かします。`rustup target add wasm32-wasip1` が必要です。

### MCP による AI からの操作

プラグインは、Model Context Protocol を通じて AI クライアント（Claude Code、Codex など）
から操作できます。MCP を知らない DAW の中でも使えます。1つのハブがコンピューター全体を
担当するので、AI クライアントで一度設定すれば、

```bash
claude mcp add ferritron -- ferritron-mcp
```

どの DAW の中でも、許可したすべての Ferritron プラグインに届きます。プラグインは
プロジェクトファイルの `[mcp]` で参加し、**AI アクセス**スイッチを持ちます。新しい
インスタンスではすべてオフです。ユーザーがスイッチをオンにしたインスタンスだけがハブに
接続します（どの DAW のプロセス、どのトラックにいるかも報告します）。AI クライアントは
それらの一覧を取得し、パラメータを読み書きし（エディタでの編集と同じように DAW に
伝わります）、プラグインの前後の音声をレベル付きの WAV ファイルとして取得できます。
ハブは 127.0.0.1 だけで待ち受け、ユーザーの Ferritron フォルダにあるトークンを提示した
プロセスだけを受け入れます。別の AI クライアントが2つ目の `ferritron-mcp` を起動すると
最初のハブに中継するので、ハブは常に1つです。

ツール: `list_plugins`、`get_parameters`、`set_parameter`、`capture_audio`。

### リソースの共有

多くのトラックに挿した同じプラグインや、同じ開発元の複数のプラグインは、同じ
インパルス応答、ウェーブテーブル、サンプルを読み込むことがよくあります。
`ferritron::share` はそれらを一度だけ作ります:

- `share::cached(key, build)`: このプラグインライブラリのインスタンス間で、どれかが
  保持している間共有される値（`Arc`）。
- `share::bytes(name, build)`: 名前付き共有メモリ上の読み取り専用のバイト列で、
  セッション全体で一度だけ作られます。他のプラグインライブラリや他のプロセス（2つ目の
  DAW、DAW のプラグインサンドボックス）も同じ物理メモリを参照します。最後の利用者が
  いなくなると OS が解放します。

GPU 上では、同じインパルス応答を使う畳み込みはそのスペクトルを自動で共有します。

### DirectLink: プラグイン同士の通信

Ferritron のプラグインは **DirectLink** チャンネルで値をやり取りできます。たとえば、
エフェクトの後ろのプラグインが前のレベルを必要とするゲインマッチのペアです:

```rust
let link = ferritron::directlink::DirectLink::open("com.example.gainmatch", "group 1")?; // in activate
let level = link.port("level").unwrap();
level.publish(&[ms]);                       // audio thread: never blocks
if let Some(r) = level.read(&mut v) { /* r.age tells how fresh */ }
```

チャンネルは名前付き共有メモリの小さな領域です。プラグインライブラリやプロセスをまたいで
動き、最後の利用者がいなくなると OS が解放します（クラッシュ後も何も漏れず、宙に浮く
こともありません）。書き込みと読み取りはロックもメモリ確保もせず、複数の書き手がいても
値が壊れることはありません。チャンネル名をパラメータか状態に保存しておけば、
プロジェクトを開き直したときに相手と自動でつながります。
[examples/gain-match](examples/gain-match) を参照してください。

### インストゥルメント

`[plugin]` に `kind = "instrument"` を設定します。プラグインは音声入力の代わりにノートを
受け取ります。`ctx.notes` には、そのブロックのノートオン／オフがサンプル位置付きで
入っています。16ボイスのサインシンセ [examples/sine-synth](examples/sine-synth) を参照して
ください。

```rust
for note in ctx.notes {
    match note.kind {
        NoteKind::On => { /* start a voice at note.offset: note.key, note.velocity */ }
        NoteKind::Off => { /* release it */ }
    }
}
```

### DAW なしでのテスト

`ferritron::testing::Harness` は、ホストと同じ経路（サンプル単位のオートメーション、
バイパス、ノート、状態）でプラグインを動かします:

```rust
let mut h = ferritron::testing::Harness::<Gain>::new(48_000.0, 512);
h.set_param("gain", -6.0);
h.automate(32, "gain", 0.0);     // change at sample 32
let (mut l, mut r) = ([1.0; 64], [1.0; 64]);
assert!(h.process(&mut [&mut l, &mut r]));
```

### パラメータの型

| `type`  | キー                                   | 読み取り       |
|---------|----------------------------------------|----------------|
| `float` | `range = [min, max]`, `default`, `unit` | `.value() -> f32` |
| `int`   | `range = [min, max]`, `default`, `unit` | `.value() -> i32` |
| `bool`  | `default = true/false`                 | `.value() -> bool` |
| `enum`  | `choices = ["A", "B"]`, `default = "A"` | `.index() -> usize` |

どのパラメータにも、表示名の `name` を任意で指定できます（省略するとキーを先頭大文字に
したもの）。キーは保存されたプロジェクトとオートメーションでのパラメータの識別子なので、
リリース後に名前を変えないでください。

### 多言語対応

[Fluent](https://projectfluent.org/) のファイルを `locales/<言語>/` に置きます:

```ftl
# locales/ja/main.ftl
param-gain = ゲイン
```

- `param-<key>` はパラメータ名を、`param-<key>-<index>` は `enum` パラメータの選択肢を
  翻訳します。翻訳のない文字列は `ja-JP` → `ja` → `en` → プロジェクトファイルの名前の
  順に代替されます。
- ファイルはコンパイル時に検査されます。Fluent の構文エラーや、どのパラメータにも
  一致しないキーは、ファイル名と行番号付きのビルドエラーになります。
- 表示言語は次の最初に見つかったものです: 環境変数 `FERRITRON_LANGUAGE`、
  `%APPDATA%\Ferritron\settings.toml` の設定、OS の言語。1つの設定ですべての
  Ferritron プラグインに適用されます:

  ```toml
  language = "ja"
  [plugins."dev.ferritron.examples.gain"]   # optional per-plugin override
  language = "en"
  ```

- `%APPDATA%\Ferritron\locales\<plugin id>\<language>.ftl` を置けば、誰でも再ビルドなしで
  言語を追加・上書きできます。
- `cargo ferritron i18n check <package>` は言語ごとに足りない文字列を一覧表示し、足りない
  ものがあれば 0 以外で終了するので、CI で使えます。
- `cargo ferritron bundle` は新しく追加した `.ftl` ファイルを検知します。通常の
  `cargo build` では、新しいファイルを追加した後に、追跡されているいずれかのファイルを
  編集してください（または `cargo clean -p <package>` を実行）。

## ビルド

### 1. 公式 SDK をご自身で入手する

**Ferritron はプラグイン形式の SDK を一切同梱していません。** 提供するのは、ご自身で
用意した SDK に対してビルドされるアダプタのコードだけです。VST3 と CLAP は既定で両方
有効です。片方だけをビルドするには
`ferritron = { ..., default-features = false, features = ["vst3"] }` を使います。

VST3 の場合:

1. Steinberg から VST3 SDK をダウンロードし
   （<https://github.com/steinbergmedia/vst3sdk> または
   <https://www.steinberg.net/developers/>）、そのライセンスに同意します。
2. `VST3_SDK_DIR` に SDK のフォルダ（`pluginterfaces/` を含むフォルダ）を指定します。
   たとえばプロジェクトの `.cargo/config.toml` で:

   ```toml
   [env]
   VST3_SDK_DIR = "C:/SDKs/vst3sdk"
   CLAP_SDK_DIR = "C:/SDKs/clap"
   ```

CLAP の場合: <https://github.com/free-audio/clap> からヘッダをダウンロードし、
`CLAP_SDK_DIR` に `include/clap/clap.h` を含むフォルダを指定します。

### 2. 必要なもの

- Rust 1.89 以降
- C++17 コンパイラ: Windows では Rust の MSVC ツールチェーンが使う Visual Studio Build Tools、Linux では GCC か Clang

**macOS**（Apple Silicon と Intel、macOS 11 以降）では Xcode のコマンドラインツール
（`xcode-select --install`）が必要です。`cargo ferritron bundle gain` は Info.plist 付きの
`<name>.vst3`、`<name>.clap`、`<name>.component`（Audio Unit）のバンドルを書き出し、署名
します（アドホック、または `FERRITRON_CODESIGN_IDENTITY` の ID で）。`--install` は
`~/Library/Audio/Plug-Ins/{VST3,CLAP,Components}` にコピーします。エディタ（Web:
WKWebView、GPU: Metal）、ホットリロード、プロセス分離、ARA、Audio Workgroup はすべて
macOS 向けの実装があります。**これらは書かれていて型検査
（`cargo check --target aarch64-apple-darwin` / `x86_64-apple-darwin`）も通っていますが、
Mac の実機ではまだビルドも実行もしていません。** 状況は
[docs/architecture.md](docs/architecture.md) を参照してください。

**Linux** も同じ方法で動きます（`cargo ferritron bundle gain` は
`<name>.vst3/Contents/x86_64-linux/<name>.so` と `<name>.clap` を書き出し、`--install` は
`~/.vst3` と `~/.clap` にコピーします）。Linux にはまだエディタがありません。ホット
リロードとプロセス分離は macOS と同じ POSIX のコードを使いますが、Linux では未テストです
（分離には `ferritron-host` ヘルパーが必要で、これは VST3 のようなディレクトリ形式の
バンドルにしか入りません）。すべてのサンプルは Linux（Docker 上の Debian と GCC で確認）
で両方のバリデータに合格しています。ソフトウェア GPU（llvmpipe、lavapipe、WARP）は GPU
なしとみなし、GPU ノードは CPU で動きます。

### 3. プラグインを作る（任意）

```bash
cargo install --path crates/ferritron-cli   # provides `cargo ferritron` everywhere
cargo ferritron new my-plugin
```

すぐにビルドできるプロジェクト（ゲインのサンプル、日本語訳、SDK 設定のひな形）が作られ
ます。プロジェクトはこの Ferritron のフォルダをパスで参照します。このリポジトリの中では、
インストールしなくても `cargo ferritron` のエイリアスが使えます。インストールした
`cargo-ferritron` は `FERRITRON_HOME` から Ferritron を探すので、このフォルダ
（`crates/ferritron` を含むフォルダ）を設定してください。

#### Ferritron Editor

**Ferritron Editor** は、JUCE の Projucer のように、プロジェクトの作成と編集をウィンドウ、
コマンドライン、AI アシスタントから行えます。このリポジトリには含まれていません。
クローズドソースで、[Releases](https://github.com/Ztraqto/Ferritron/releases) から単体で、
または Ferritron 本体と一緒になったスターターキットとしてダウンロードできます。

```bash
ferritron-editor new my-synth --template instrument --vendor "Acme"
cd my-synth
ferritron-editor param add cutoff --type float --min 20 --max 20000 --default 1000 --unit Hz
ferritron-editor feature mcp on
ferritron-editor info
ferritron-editor bundle
```

- `cargo ferritron new` と同じプロジェクトを作り、すべての編集を、書き込む前にビルドと
  同じ規則（`ferritron-manifest`）で検査します。不正な編集は何も変更せず、プロジェクト
  ファイルのコメントは保持されます。
- **GUI**（Tauri 2、Apple の Human Interface Guidelines に準拠）: 複数のプロジェクトを
  同時に開き、パラメータ、機能、ビルドを扱えます。英語と日本語に対応しています。
- **AI:** `ferritron-editor mcp` は stdio の MCP サーバーです
  （`claude mcp add ferritron-editor -- ferritron-editor mcp`）。
- **プロジェクトのロック:** プロジェクトを1つのファイルに暗号化し、パスキー（Windows
  Hello、セキュリティキー）かリカバリーコードでしか開けないようにできます。Editor で
  開いている間だけ解除されます。
- 新しいプロジェクトは、`FERRITRON_HOME`、または Editor の隣（スターターキットの構成）に
  ある Ferritron 本体を参照します。

### 4. ビルドとバンドル

```bash
cargo ferritron bundle gain
```

`gain` パッケージをリリースモードでビルドし、`target/bundled/Ferritron Gain.vst3` と
`target/bundled/Ferritron Gain.clap`（macOS では `Ferritron Gain.component` も）を書き出し
ます。これらを `C:\Program Files\Common Files\VST3\` と `...\Common Files\CLAP\` に
コピーするか、管理者のシェルで `--install` を付けて実行してください。その後、DAW で
プラグインを再スキャンします。

#### Audio Unit（macOS）

`au` 機能（既定で有効。他のシステムでは何もビルドしません）は AU v2 のアダプタを追加
します。AU v2 は macOS の C インターフェイスなので、SDK は不要です。コンポーネントの
登録コードは、既定では `aufx` / `aumu`（エフェクト、インストゥルメント）、`plugin.id` から
導いたサブタイプ、`plugin.vendor` から導いたメーカーコードです。変わらないように明示
してください:

```toml
[au]
type = "aufx"          # aufx (effect), aumu (instrument), aumf (effect taking MIDI)
subtype = "Gain"       # 4 characters, unique among your plugins
manufacturer = "Ztrq"  # 4 characters, at least one not lowercase
```

`--install` の後、`auval -v aufx Gain Ztrq` でバンドルを確認できます。対応するホストは
Audio Workgroup（レンダーコンテキストのオブザーバー）を渡し、Ferritron のワーカー
スレッドがそこに参加します。

#### Windows や Linux から macOS のコードを検査する

```bash
rustup target add aarch64-apple-darwin
FERRITRON_SKIP_NATIVE=1 cargo check --workspace --target aarch64-apple-darwin
```

`FERRITRON_SKIP_NATIVE` は C、C++、Objective-C の部分を飛ばし（コンパイルする Apple の
ツールチェーンがないため）、すべての Rust コードを macOS 向けに型検査します。C を
コンパイルする依存クレート（wasmtime）は、そのターゲット用のコンパイラを必要とするので、
`CC_aarch64_apple_darwin` に clang を指定してください。

### ホットリロード（開発用）

DAW を再起動せずに、動作中のプラグインを更新できます:

```bash
cargo ferritron bundle gain --dev --install
```

開発用バンドルを一度インストールして DAW で読み込みます。その後は、

```bash
cargo build -p gain
```

を実行するたびに、2つのオーディオブロックの間で新しいコードが動作中のプラグインに
差し替えられます。パラメータの値と状態は引き継がれます。リロードのログは
`%TEMP%\ferritron-hot-reload\<package>.log`（macOS では `$TMPDIR/ferritron-hot-reload/`）
に記録されます。

- ホットリロードはデバッグビルドにだけ組み込まれます。リリースのバンドルはプラグインを
  直接呼び出します。
- パラメータの一覧が変わった場合は、新しいパラメータを表示するためにプラグインを挿し
  直してください。
- プラグインのコードがパニックすると、プラグインは無音を出力します。コードを直して
  再ビルドすると、読み込み直したコードで処理が再開します。

DAW なしで試すには、最小のテストホストでバンドルを動かし、その間に再ビルドします:

```bash
cargo run -p ferritron-testhost -- "target/bundled/Ferritron Gain.vst3" --seconds 60
```

### 検証

- VST3: VST3 SDK の `validator` ターゲットを CMake でビルドし、
  `validator -e "target/bundled/Ferritron Gain.vst3"` を実行します。
- CLAP: [clap-validator](https://github.com/free-audio/clap-validator) をビルドし、
  `clap-validator validate "target/bundled/Ferritron Gain.clap"` を実行します。とても軽い
  プラグインでは、Ferritron が処理中にフラッシュ・トゥ・ゼロを有効にしていても、
  `process-audio-denormals` のチェックが警告を出すことがあります。計測時間に、
  バリデータ自身がデノーマルのテストデータを生成する時間が含まれるためです。

## プラグイン形式の追加

形式は1つのクレートです。Ferritron のコアも CLI も変更しません:

1. **エントリポイント**: ホストが探すシンボルをエクスポートし、`ferritron_plugin_entry()`
   からプラグインを取得します（`ferritron_plugin_api` の表。
   `crates/ferritron-core/include/ferritron_abi.h` を参照）。通常は、開発者が用意した
   形式の SDK に対してコンパイルする小さなシムです。
2. **パッケージング**: クレートの `Cargo.toml` で宣言します:

   ```toml
   [package.metadata.ferritron.format]
   name = "VST3"
   entry-symbol = "GetPluginFactory"                 # how bundle detects the format
   root = "{name}.vst3"                              # packaged and installed item
   binary = "{name}.vst3/Contents/{arch}/{name}.vst3"
   arch = { windows-x86_64 = "x86_64-win" }          # values for {arch}
   install = { windows = "{COMMONPROGRAMFILES}/VST3" }
   ```

3. **リンク**: 組み込みの形式は `ferritron` クレートの機能です。サードパーティの形式は、
   プラグインから `use ferritron_xyz as _;` でリンクします。

`crates/ferritron-vst3` と `crates/ferritron-clap` はまさにこの方法で書かれています。

## リポジトリの構成

| パス | 役割 |
|---|---|
| `crates/ferritron` | プラグインが依存するクレート（再エクスポート、prelude） |
| `crates/ferritron-core` | 形式に依存しない API と C ABI（`include/ferritron_abi.h`） |
| `crates/ferritron-macros` | `#[ferritron::plugin]`: プロジェクトファイル（`.frtx`）を読み、コードを生成 |
| `crates/ferritron-vst3` | VST3 アダプタ: ご自身の VST3 SDK に対してコンパイルする C++ のシム |
| `crates/ferritron-clap` | CLAP アダプタ: ご自身の CLAP ヘッダに対してコンパイルする C のシム |
| `crates/ferritron-au` | Audio Unit v2 アダプタ（macOS）、Rust 製 |
| `crates/ferritron-web` | HTML/JS エディタのバックエンド（wry による WebView2 / WKWebView） |
| `crates/ferritron-ara` | ARA 2: ご自身の ARA SDK 上のドキュメントコントローラー、`AraRender` |
| `crates/ferritron-gpu-view` | wgpu で描くエディタ（`[view] backend = "gpu"`） |
| `crates/ferritron-cli` | `cargo ferritron bundle` |
| `crates/ferritron-build` | ビルドスクリプト用の補助: SDK の検出、`FERRITRON_SKIP_NATIVE` |
| `tools/testhost` | DAW なしでテストするための最小の VST3 ホスト |
| `crates/ferritron-gpu` | GPU 演算のバックエンド（`Convolver`、IIR バンク、グラフのノード。wgpu） |
| `crates/ferritron-slang` | ビルド時の Slang カーネルコンパイラ（WGSL + C++） |
| `bindings/csharp` | C# バインディング（`Ferritron.cs`） |
| `crates/ferritron-wasm` | 分離レベル 2: wasmtime サンドボックス内のプラグイン |
| `crates/ferritron-vulkan` | ネイティブ Vulkan: パーシステントカーネル、Tensor コア（Slang シェーダー、SPIR-V 同梱） |
| `crates/ferritron-graph` | ノードグラフ: レイテンシ補正、CPU/GPU の自動配置 |
| `examples/gpu-reverb` | GPU 畳み込みリバーブのサンプル |
| `examples/ara-reverse` | オーディオファイルを逆再生する ARA 2 プラグイン |
| `examples/gpu-view` | エディタを GPU（wgpu）で描くゲイン |
| `examples/slang-drive` | Slang カーネルのサチュレーター（1つのソースから CPU と GPU） |
| `examples/gpu-chain` | Ferritron Chain: 1つのグラフの3つのモジュールを境界を越えて融合 |
| `examples/cpp-gain` | `ferritron.hpp` を使って C++ で書いたプラグイン |
| `examples/csharp-gain` | C# で書いたプラグイン（NativeAOT のコンパニオンライブラリ） |
| `examples/wasm-sandbox` | 分離レベル 2: WebAssembly サンドボックス内の `examples/wasm-crusher` |
| `examples/gain-match` | DirectLink: エフェクトを挟むゲインマッチのペア |
| `examples/gpu-modal` | GPU モーダルシンセサイザーのサンプル（インストゥルメント、レイテンシを報告） |
| `tools/ferritron-mcp` | AI クライアント用の MCP ハブ（コンピューターに1つ） |
| `crates/ferritron-manifest` | プロジェクトファイル（`.frtx`）の検出、解析、新規プロジェクトのテンプレート。マクロ、CLI、Ferritron Editor で共有 |
| `tools/bench` | ランタイムのベンチマーク（並列化の高速化、GPU 畳み込み） |
| `tools/crash-fixture` | プロセス分離のテスト用に、指示でクラッシュするプラグイン |
| `examples/gain` | 最小のエフェクトのサンプル |
| `examples/sine-synth` | インストゥルメントのサンプル（ポリフォニックのサインシンセ） |
| `docs/architecture.md` | 設計書（日本語） |

## バージョン

Ferritron のすべてのクレートは1つのバージョンを共有します（現在 **1.1.0**）。
[CHANGELOG.md](CHANGELOG.md) を参照してください。プラグインでは
`ferritron = { path = "...", version = "1.1.0" }` で固定できます。`ferritron::VERSION` と
`cargo ferritron --version` で確認できます。

## ライセンス

Ferritron は次のいずれかのライセンスで提供します。どちらを選ぶかは利用者の自由です。

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE)）
- MIT license（[LICENSE-MIT](LICENSE-MIT)）

明示的な別段の意思表示がない限り、Ferritron に含めるために意図的に提出された貢献は、
Apache-2.0 ライセンスの定義に従い、追加の条件なしに上記の二重ライセンスで提供される
ものとします。

プラグイン形式の SDK（VST3、CLAP、ARA、Audio Unit）は Ferritron に含まれていません。
開発者がご自身で入手し、それぞれの提供元のライセンスに従ってください。

VST は Steinberg Media Technologies GmbH の欧州およびその他の国における登録商標です。
Audio Unit は Apple Inc. の商標です。ARA は Celemony Software GmbH の商標です。
Ferritron はこれらの企業と提携しておらず、承認を受けたものでもありません。
