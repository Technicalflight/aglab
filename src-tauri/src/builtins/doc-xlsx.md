# 电子表格

创建、编辑与审阅 .xlsx。工具链是 Python + openpyxl；没有就先探测、征得同意再装（`pip install openpyxl`）。

## 第零步：探测工具链

`python -c "import openpyxl; print(openpyxl.__version__)"`——缺了先征求同意再装。run_command 60 秒强杀，装不上分步装或让用户自己装。要跑本地预览服务器这类长活，给 run_command 传 background=true（然后用 command_output 看输出、command_stop 收尾），它不套 60 秒。

## 创建

```python
from openpyxl import Workbook
from openpyxl.styles import Font, PatternFill

wb = Workbook()
ws = wb.active
ws.title = "汇总"
ws.append(["月份", "销售额"])
ws["B2"] = 1234.5
ws["A1"].font = Font(bold=True)
ws.freeze_panes = "A2"          # 冻结表头
ws.auto_filter.ref = ws.dimensions
wb.save("产出.xlsx")
```

- 公式直接写字符串：`ws["C2"] = "=B2*1.13"`。公式在 Excel 打开时才算——openpyxl 读不回计算结果（见下）。
- 图表：`from openpyxl.chart import BarChart; chart.add_data(...); ws.add_chart(chart, "E2")`。

## 编辑

- `load_workbook("已有.xlsx")` 打开。**公式陷阱**：默认读回的是公式串；`load_workbook(path, data_only=True)` 读的是 Excel 上次保存时缓存的结果——文件从没被 Excel 打开过，缓存就是空的（读回 None）。要"读值改数"就开两份：data_only 的读值，普通的那份改写。
- .xlsm 带宏：`load_workbook(path, keep_vba=True)`，忘了这参数宏就没了。
- CSV 够用就别用 xlsx：纯数据交换用 csv 模块，打开快、没有格式陷阱。

## 审阅

出一份盘点报告：工作表清单、每张的行列规模、表头几行、公式格清单、合并单元格。给用户核对"表里到底有什么"。数字核对用 data_only 那份；None 说明这格没被 Excel 算过，照实说。

## 红线

- 脚本改的文件**不走**写文件回滚快照：动手前先复制原件。
- 输出 32KB 截断：大表的盘点写进报告文件，别全 print。
