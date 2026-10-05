# 搜图

查找插图与参考配图。**不新增任何网络通道**：复用读网页工具（web_fetch）查免密钥的开放图库 API，拿到 JSON 文本后按授权字段汇报。

## 数据源

首选 Openverse（聚合各开放授权图库，免密钥）：

```
https://api.openverse.org/v1/images/?q=城市+夜景&page_size=10
```

- 查询词 URL 编码、空格写 `+`；`page_size` 别超过 20——匿名请求有速率限制，连发会 429。
- 备选 Wikimedia Commons：

```
https://commons.wikimedia.org/w/api.php?action=query&generator=search&gsrsearch=filetype:bitmap+城市夜景&gsrlimit=10&prop=imageinfo&iiprop=url|extmetadata&format=json
```

## 流程

1. 用 web_fetch 抓上面的 URL。返回的是 JSON 文本（标题会是「（无标题）」，正文就是数据），从中读 `results[]` 的字段：`title`、`url`（图片直链）、`license`、`creator`、`foreign_landing_url`（来源页）。
2. 挑最相关的几张汇报，**每张必须带三件套：授权类型、作者、来源页链接**——开放授权（CC 系列）的最低要求就是署名，漏了等于教用户侵权。
3. 授权字段是 `by-nc` 这类缩写时翻成人话：NC = 非商业，ND = 不得改作，用户要拿去商用就得挑没有 NC/ND 的。

## 边界

- **图片本体不抓来预览**：读网页工具拒二进制，图片直链抓了只会报错。给用户直链与来源页，让人自己看。用户明确要落盘再说（用命令行 curl 下载到项目里，授权三件套一起记进说明文件）。
- 429 / 超时：如实报"图库限流了，稍后再试"，不换歪门源。
- 出口域名名单收紧时（设置 → 权限），这两个 API 域不在名单里就发不出去——报错原话会说明是哪家不在名单，引导用户去放行或清空名单。
- 找"能商用的高清素材"这类需求：Openverse 支持 `license_type=commercial` 参数，主动带上。
