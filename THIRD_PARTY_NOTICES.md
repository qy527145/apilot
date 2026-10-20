# 第三方组件与数据声明

Apilot 的「指纹检测」功能移植自 **LM Fingerpoint Detector**（lm-detector），并复用了它的
冻结参考库数据。以下内容随安装包一同分发（编译进二进制）：

| 文件 | 来源 | 内容 |
|---|---|---|
| `src-tauri/assets/fingerprint/shared_detector.bin.zst` | lm-detector `data/shared_detector.bin` | 冻结排名器与校准参数（float32 数值特征，不含原始回答文本） |
| `src-tauri/assets/fingerprint/models.json` | 由 lm-detector `data/unified_bank.json` 派生 | 模型展示元数据 |
| `src-tauri/assets/fingerprint/challenges.json` | lm-detector `data/enrollment-suite.json` | 36 道固定挑战提示词 |

- 上游仓库：https://github.com/Ikaleio/lm-detector
- 内置快照 commit：`70410db4522387c7d4a6dff8c7a4b6ce0f0239f1`
- 内置参考库构建时间：`2026-10-17T09:36:07.798950+00:00`
- 详细文件哈希与更新步骤见 `src-tauri/assets/fingerprint/PROVENANCE.md`。

上游 README 还致谢了 [xqy2006/ModelTrace](https://github.com/xqy2006/ModelTrace) 提供算法思路
参考和部分原始数据。本项目保留该致谢。

## MIT License

Copyright (c) 2026 xqy2006

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
