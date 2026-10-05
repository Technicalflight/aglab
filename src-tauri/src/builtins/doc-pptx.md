# 演示文档

创建、编辑与审阅 .pptx。工具链是 Python + python-pptx；没有就先探测、征得同意再装（`pip install python-pptx`）。

## 第零步：探测工具链

`python -c "import pptx; print(pptx.__version__)"`——缺了先征求同意再装。run_command 60 秒强杀，装不上分步装或让用户自己装。要跑本地预览服务器这类长活，给 run_command 传 background=true（然后用 command_output 看输出、command_stop 收尾），它不套 60 秒。

## 创建

```python
from pptx import Presentation
from pptx.util import Pt, Cm

prs = Presentation()                 # 或 Presentation("模板.pptx") 带母版与主题
layout = prs.slide_layouts[1]        # 标题+内容 版式
slide = prs.slides.add_slide(layout)
slide.shapes.title.text = "季度回顾"
body = slide.placeholders[1]
body.text = "第一点"
p = body.add_paragraph(); p.text = "第二点"; p.level = 1
slide.notes_slide.notes_text_frame.text = "演讲者备注"
prs.save("产出.pptx")
```

- 想要好看的版式：让用户提供一份模板 pptx，`Presentation("模板.pptx")` 起步，母版主题都跟着走——从零新建的默认版式很素，先说清这一点再动手。
- 页数 = `len(prs.slides)`，这个是真的，可以报。

## 编辑

- 逐页逐形状：`for slide in prs.slides: for shape in slide.shapes:`；文本在 `shape.text_frame.paragraphs[].runs[]`。
- 找标题：`slide.shapes.title`；占位符按 `shape.placeholder_format.idx` 认。
- 表格与图表：`shape.has_table` / `shape.has_chart`；图表数据可改 `chart.replace_data()`（需要 category/chart data 引用）。
- 跨 run 替换丢格式的坑与 Word 一样：优先逐 run 替换，找不到再整段重写。

## 审阅

导出大纲报告：每页标题 → 各形状文本 → 备注，一页一段。给用户核对"这套片子里到底写了什么"。SmartArt 与纯图页读不出文本——报告里标"此页无可提取文本"，别装作看过。

## 红线

- 脚本改的文件**不走**写文件回滚快照：动手前先复制原件。
- python-pptx 不能渲染缩略图：给不了"这页长什么样"，只能给文本结构——用户要看效果得自己开 PowerPoint。
