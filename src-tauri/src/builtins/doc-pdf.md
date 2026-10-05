# PDF

创建、编辑与审阅 PDF。PDF 按任务拆工具：读与合并拆分用 pypdf，从零生成用 reportlab 或 fpdf2。没有 Python 就先探测、征得同意再装。

## 第零步：探测工具链

`python --version`；`python -c "import pypdf"`（读取/组装）、`python -c "import reportlab"`（生成）。缺哪个征求同意后 `pip install pypdf` / `pip install reportlab`。run_command 60 秒强杀：装不上分步装或让用户自己装。要跑本地预览服务器这类长活，给 run_command 传 background=true（然后用 command_output 看输出、command_stop 收尾），它不套 60 秒。

## 读取与审阅（pypdf）

```python
from pypdf import PdfReader
reader = PdfReader("文件.pdf")
print(len(reader.pages), "页")
for i, page in enumerate(reader.pages):
    text = page.extract_text() or ""
```

- 审阅报告按页给文本与元数据（标题/作者/页数）；`extract_text()` 返回空串的页多半是**扫描图**——没有文本层，如实告诉用户"这份 PDF 是扫描件，要读内容得 OCR，这一步我做不了"，别编。
- 表单域：`reader.get_fields()`。

## 组装（pypdf）

合并、拆分、抽页、旋转、加密：

```python
from pypdf import PdfWriter, PdfReader
writer = PdfWriter()
for path in ["a.pdf", "b.pdf"]:
    for page in PdfReader(path).pages:
        writer.add_page(page)
with open("合并.pdf", "wb") as f:
    writer.write(f)
```

## 从零生成

**首选 reportlab**——内置中日韩 CID 字体，不用找字体文件：

```python
from reportlab.pdfgen import canvas
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
c = canvas.Canvas("产出.pdf")
c.setFont("STSong-Light", 14)
c.drawString(72, 800, "中文标题")
c.showPage(); c.save()
```

**备选 fpdf2**（API 更顺，但要本机字体文件）：`pip install fpdf2`；`pdf.add_font("hei", "", r"C:\Windows\Fonts\simhei.ttf")`——核心 14 字体不含中文，**不挂字体直接写中文会乱码或报错**；Windows 自带 C:\Windows\Fonts 下挑一个 .ttf（simhei.ttf、msyh.ttf），fpdf2 ≥2.7 也认 .ttc。

- 排版复杂（图表混排、精确版式）就先生成中间格式再转：有 pandoc 的话 `pandoc 文档.md -o 产出.pdf`（中文要配 xelatex 或 wkhtmltopdf 引擎，报错就直说缺引擎）。

## 红线

- 脚本改的文件**不走**写文件回滚快照：覆盖前先复制原件。
- 输出 32KB 截断：长文提取写进 txt/md 文件再让模型 read_file，别全 print。
- 解密：带口令的 PDF 用户不给口令就不碰；`PdfReader("x.pdf", password="…")` 只在用户明说了口令时用。
