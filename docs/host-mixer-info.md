# ホストのミキサー情報（E11 の調査結果）

Ferritron 2.0.0 で `TrackInfo` に入る情報と、形式ごとに取れるもの。SDK のヘッダー（VST3 SDK 3.8.0、CLAP 1.2 系）で確かめた範囲で書く。実際の DAW がどれを送るかは確かめていない。

| 情報 | `TrackInfo` | VST3（`IInfoListener`） | CLAP（`track-info`） | ほかの形式 |
|---|---|---|---|---|
| 名前 | `name` | `kChannelNameKey` | `name` | AU は `kAudioUnitProperty_ContextName`（Ferritron の AU は未対応・未検証） |
| 色 | `color` | `kChannelColorKey` | `color` | — |
| ミキサー上の位置 | `index` | `kChannelIndexKey` | なし | — |
| トラックの種類（通常・リターン・バス・マスター） | `kind`（2.0） | なし | `IS_FOR_RETURN_TRACK` / `IS_FOR_BUS` / `IS_FOR_MASTER` | — |
| 挿した位置（フェーダー前・後・パンナー） | `location`（2.0） | `kChannelPluginLocationKey` | なし | — |
| トラックのチャンネル数 | `channels`（2.0） | なし | `audio_channel_count` | — |
| ミュート・ソロ・フェーダー値・パン | なし | **なし** | **なし** | LV2・AAX・VST2 にも共通の仕組みはない |

## 結論

- ミュート、ソロ、フェーダーの値は、どの形式にもプラグインへ伝える仕組みがない。PanStudio の要望（DAW のフェーダー・ミュート・ソロを見る）は、形式の制約で**不可**。
- 取れるものは `Option` で足した（`kind`・`location`・`channels`）。ホストが送らなければ `None`。
- VST3 の `kChannelIndexNamespace*`（フォルダ名など）と `kChannelImageKey` は、要望の具体例が出たら足す。
- 確認方法: `examples/instances` のテストで、ホストが送った値が `InstanceHandle::track()` に出ることを確かめた（Harness 経由）。実 DAW での確認はしていない。
