# Word 文档

创建、编辑与审阅 .docx。工具链是 Python + python-docx；机器上没有就先探测、征得同意再装。

## 第零步：探测工具链

1. `python --version`——没有 Python 就直说：请先装 Python（python.org 或 winget install Python.Python.3.12），不要静默换别的方案。
2. `python -c "import docx; print(docx.__version__)"`——报 ModuleNotFoundError 就征求同意后 `pip install python-docx`。注意 run_command 60 秒强杀：装不上就分步装，或让用户自己在终端装。要跑本地预览服务器这类长活，给 run_command 传 background=true（然后用 command_output 看输出、command_stop 收尾），它不套 60 秒。

## 创建

把生成脚本用 write_file 落到项目里（比如 `make_doc.py`），再 `python make_doc.py` 跑。骨架：

```python
from docx import Document
from docx.shared import Pt, Cm

doc = Document()
doc.add_heading("标题", level=1)
doc.add_paragraph("正文段落。")
table = doc.add_table(rows=2, cols=3)
table.style = "Table Grid"
doc.add_picture("chart.png", width=Cm(12))
doc.save("产出.docx")
```

中文字号与样式：改 `style.font.name` 时要同时设 `style.element.rPr.rFonts.set(qn("w:eastAsia"), "微软雅黑")`，只设 name 对中文不生效。

## 编辑

- `Document("已有.docx")` 打开后逐段处理：`doc.paragraphs`（正文段）、`table.rows`（表格）。改完**另存**或覆盖前先备份。
- 跨 run 替换会丢格式：一段文字可能被拆成多个 run（拼写检查、局部加粗都会拆）。整段替换 `paragraph.text = 新文` 最稳但丢段内格式；要保格式就逐 run 找子串替换，找不到再升级到整段。
- 逃生通道：.docx 本质是 zip 包 XML。怪功能（域代码、复杂域、python-docx 没暴露的属性）可以解包改 XML 再打包——先备份，改完让用户开 Word 验证。

## 审阅

提取全部文本与结构出一份报告：标题层级、每段字数、表格清单。给用户核对"文档里到底有什么"用。页数别报——python-docx 拿不到分页，那是排版结果；报字数与段落数。

## 红线

- 脚本改的文件**不走**写文件回滚快照（快照只罩 write_file/edit_file 工具）：动手前先复制一份原件（`copy 产出.docx 产出.bak.docx`）。
- run_command 输出超 32KB 截断：脚本里 print 要节制，报错写到文件再读。
