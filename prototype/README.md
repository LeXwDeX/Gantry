# 界面原型源码

这里是 [Gantry 控制台原型](https://claude.ai/artifact/AFWjX1VuDkwHvxfNLC5t29) 的源码备份，界面中全部为示例数据。文字版规格见 [docs/prototype.md](../docs/prototype.md)。

| 文件 | 画板 |
| --- | --- |
| `Main.dc.html` | 桌面端 · 工作区（对话 / 关系地图 / 工作树 + 左栏 + 检视器），可交互 |
| `Settings.dc.html` | 桌面端 · 设置（模型页） |
| `Mobile.dc.html` | 手机端 · 对话 |
| `canvas.json` | 画布索引（画板位置与尺寸） |

文件是 Design Component 格式（HTML + `renderVals()` 逻辑），依赖画布运行时，不能直接用浏览器打开。实现 `desktop-ui` 时把它当作交互与视觉的参考，不要直接复用代码。
