# 百积木本地浏览器连接器

`com.baijimu.connector.browser` 是由 Bridge Agent 托管的客户端本地应用。它在用户设备上启动 Chrome 或 Edge，并把页面读取与交互能力以有约束的 HTTP 方法提供给已授权工作区。

## 设计边界

- Connector 只监听回环地址，不开放局域网或公网端口。
- 浏览器 Profile 按可信的 `X-Baijimu-Workspace-Id` 隔离，Cookie、LocalStorage 等状态保存在 `BAIJIMU_CONNECTOR_DATA_DIR`。
- 每个 Profile 必须配置允许访问的域名；导航拒绝非 HTTP(S)、带凭据 URL、回环、私网、链路本地及未授权目标。
- 文件上传只能读取管理策略中显式授权目录的规范化文件路径。
- 不提供原始 JavaScript、Chrome DevTools Protocol、Shell 或任意命令执行接口。
- 同一 Profile 同时只能有一个浏览器会话，关闭会话后登录态继续保留。

网站专属选择器、发布步骤和风控规则不属于 Connector。它们应由独立 Skill 或工作流维护，避免把视频号或其他网站的可变页面事实硬编码进本地运行时。

## 本地开发

需要 Rust 1.85+，并安装 Chrome、Chromium 或 Edge。

```bash
cargo test --locked
npm test
cargo build --locked --release
```

启动 Connector：

```bash
BAIJIMU_CONNECTOR_DATA_DIR=/absolute/private/data/directory \
  cargo run -- start
```

健康检查：

```bash
curl http://127.0.0.1:18121/healthz
```

所有远程能力、风险声明、超时及输入 Schema 以 [connector.json](./connector.json) 为准。

## 状态生命周期

- 创建：`createProfile` 创建工作区隔离元数据和浏览器数据目录。
- 更新：浏览器自身更新登录态；`updateProfile` 更新域名授权。
- 重启/升级：安装目录可替换，私有数据目录保持不变。
- 清理：管理操作关闭活动会话后删除指定 Profile；卸载默认不删除私有数据，用户可显式选择彻底清除。
- 恢复：损坏或身份不匹配的元数据会失败关闭，不跨工作区回退或自动共享。

## 发布

版本遵循 Semantic Versioning 2.0.0。唯一发布入口是仓库的 `release` 工作流：多平台构建和测试、代码签名、不可变制品上传、市场版本创建、提交审核和市场回读。禁止手工修改市场数据库或覆盖已经发布的制品 URL。

