# 文档迁移对照

> 2026-10-04；仅仓库维护，不发布。

每行覆盖一个旧正式文档的二级章节，含工作区 EMBEDDING 新增性能内容。SHA-256 标识迁移前章节，便于核对；对应内容经过主题拆分、事实修正与重复定义收敛。旧引言、目录和相关链接由新总览及统一清单取代。未跟踪评审、benchmark 与图像设计草案未纳入公开内容。

维护者的运行时套件与结果已纳入仓库：[performance.md](development/performance.md)保留方法和索引，原 TUI 测量正文完整转入[日期记录](development/benchmarks/tui-2026-10-04.md)，运行时结果及脱敏证据另存[2026-10-03 记录](development/benchmarks/runtime-2026-10-03.md)。以下迁移前摘要不变；性能入口继续承接旧章节。

| 来源标识 | 章节（原行） | 去向 | 原章节 SHA-256 |
|---|---|---|---|
| USAGE | 官网与文档（19） | [development/web-and-site.md](development/web-and-site.md) | `1abaef5641837ecef4149656cc269c5de493851c1d8754f50fefd6ee273c4c8e` |
| USAGE | Web 开发工作台（37） | [guides/web.md](guides/web.md) | `2b043036c8947728e922c066d7c626ba42f9b1b3ade5f6e99ca5629c9b4a0acc` |
| USAGE | 终端模式与操作（72） | [guides/terminal.md](guides/terminal.md) | `94deba731e1bfe803ce5dcdd380635a01c3c460e4222f3f759a769809f3d9000` |
| USAGE | 配置与参数（238） | [reference/configuration.md](reference/configuration.md) | `be47f2f9a78a2d722871645311518b7356e30374d96f5c85f91e97e7752709d5` |
| USAGE | mink-server：Web 工作区服务器（503） | [guides/web.md](guides/web.md) | `500f4c31e1d9dea1e8fba3bd879b11ffc196c8ef6478e45b2d368b0c1b59e680` |
| USAGE | 沙箱与安全（518） | [guides/security.md](guides/security.md) | `c264b380f4dbc8c010d56a00154d78886bdf253e979fbec22117ac1e20738965` |
| USAGE | 会话管理（598） | [guides/sessions-and-guidance.md](guides/sessions-and-guidance.md) | `5150708d193fe27c905c653bf8ac1dd1619025743f4d9aedd96aa3fedd5bd1ea` |
| USAGE | 计划系统（Plan）（652） | [guides/plans-and-todos.md](guides/plans-and-todos.md) | `aead7eb57df37197cab26aa731cf531965e1734d482e4d56c38afbc1fd8a10f4` |
| USAGE | 上下文压缩（673） | [guides/context-and-usage.md](guides/context-and-usage.md) | `48ebc54b5d18f9e3e029494e70f2a19414786ebfedfcc69d4560ba49b143e807` |
| USAGE | 维修流水线（737） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `842313b7c9614bfe28c47ac5c28dfe1fb30971b93434b0913b25d9379dd861d0` |
| USAGE | 工具系统（751） | [reference/tools.md](reference/tools.md) | `6f796c330d1d639ce4f268063ea5b55822251cd9bebd1323bf9f1366728f6c67` |
| USAGE | Skills（技能）（813） | [guides/customization.md](guides/customization.md) | `9e86093c2ece769a6e18b4ea3ac44c8d4ea0972c62750b8a3c84b63afa15e560` |
| USAGE | MISSION（自定义系统提示词）（849） | [guides/customization.md](guides/customization.md) | `3cd9251bbd0bfdaf22355ee3a77641136995c4058600b8e18c79f8252d323d21` |
| USAGE | SubAgent（子代理）（896） | [guides/customization.md](guides/customization.md) | `ff2efde1577f7f541048862521cae20e377ecaf46fb1f53d5941341a161e544f` |
| USAGE | 故障排查（926） | [guides/troubleshooting.md](guides/troubleshooting.md) | `43bae661f23320f3fe5fff53780af217ab9eab8b026339ce5806f0bb37c8e38c` |
| ARCHITECTURE | 官网与文档站（13） | [development/web-and-site.md](development/web-and-site.md) | `601a2ec4fdaff64a9d227b8c1f26d72f1141a41bf0101b8bd1f730e3be17bf08` |
| ARCHITECTURE | 会话工作台与人类输入（38） | [development/web-and-site.md](development/web-and-site.md) | `c7e414456dab8d9a02048d795df3f0f4420d663dbdbef19cedb8f96280a80a56` |
| ARCHITECTURE | TUI 调度与布局（73） | [development/tui.md](development/tui.md) | `88538ea2ea127acd1f00b5bfb3334aa2f879dc1c6c62b2a40f89b6f293a66228` |
| ARCHITECTURE | 项目定位（104） | [start/overview.md](start/overview.md) | `c64e1b712e1065677aa39190fba6c0f6c2cd790c22de9acfdc671a16b1c08fe9` |
| ARCHITECTURE | 核心原则（125） | [concepts/architecture.md](concepts/architecture.md) | `32ab9bac50286b4c7bf86a26e833274ee7dac4f890a239f4e0390a6271054b05` |
| ARCHITECTURE | 运行时分层（138） | [concepts/architecture.md](concepts/architecture.md) | `50d1db082aa871ef3013c2e8a41861755ddc56c33044074b2001b3f2d66fb02a` |
| ARCHITECTURE | 核心数据流（224） | [concepts/runtime.md](concepts/runtime.md) | `489e5f4b73705178c6aa168f2abce9ce1b01642a1107f6a69d4635baaf363a7d` |
| ARCHITECTURE | 模块职责（306） | [concepts/architecture.md](concepts/architecture.md) | `5fe53b68801f537ffe548b9bd74949a6543cca513c8e9ba44b20c8eff9cd3013` |
| ARCHITECTURE | Runtime 事件接口（550） | [integration/extensions.md](integration/extensions.md) | `1bacc7e90e869efb1382a817f098a927357b32a46a58c3410ca0c6230d51df0f` |
| ARCHITECTURE | Session 结构（572） | [concepts/state-and-context.md](concepts/state-and-context.md) | `91812314257c08a35bca4cd78f4d66e27f19812ef4f1b48d29e8127da78f4bc7` |
| ARCHITECTURE | 决策与拒绝的替代方案（628） | [concepts/architecture.md](concepts/architecture.md) | `e5af41c16df9e97ee5bfafcfa8819d7d9dc01fffb606fea25043786901dd9001` |
| ARCHITECTURE | 关键不变式（639） | [concepts/architecture.md](concepts/architecture.md) | `b6bf37963f0cee18bb67d9f3ee7152d1ebb2be4ff0b9db3f76b6d6471bf86e4d` |
| DESIGN | 官网的信息与交互（21） | [development/web-and-site.md](development/web-and-site.md) | `032f0685b1bc4f60f2fcbae81b04a2a167076824a02da89caf2c637026eed4fd` |
| DESIGN | TUI 阅读、编辑与异步反馈（41） | [development/tui.md](development/tui.md) | `a336c8f15c0e90a3d6fd3440e635c6fb002174a8be660e96efab379ad4c94635` |
| DESIGN | 会话工作台：安全边界与可恢复输入（70） | [concepts/state-and-context.md](concepts/state-and-context.md) | `b5239f496d94afb5dc5a681b4d0a84418bbc20b470f43150d880580aa2636161` |
| DESIGN | 主题一：Agent 主循环（107） | [concepts/runtime.md](concepts/runtime.md) | `37320913e9eef10b0476d159a166bd8627a3a279e7251480cd1f1528d6c57845` |
| DESIGN | Edit 协议绑定（202） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `2bc3617b73b57f6d94656e30ffb51fe3ea5f37cf0947f7243d9b5d5bfb00b444` |
| DESIGN | 主题二：内存模型（215） | [concepts/state-and-context.md](concepts/state-and-context.md) | `0bbe9b531c0dbfb2c57652f7feb645f0edff7cd84185ad441777b46c46e3e7b9` |
| DESIGN | 主题三：上下文压缩（335） | [concepts/state-and-context.md](concepts/state-and-context.md) | `ab4ae836b56c1f66b7c05d9362fb10376dc28347b60035d9f51b2167ffbcb7cb` |
| DESIGN | 主题四：维修流水线（451） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `3950040965399811fe8a2e58b64e6204c4c2edd43ea87fd960098a3eb9c2e6a1` |
| DESIGN | 主题五：信号驱动的信念系统（526） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `c9340d8eccadabad8dbb22fce68c7031f3ea10545086daac95937430b973b36a` |
| DESIGN | 主题六：工具执行模型（551） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `e192d7960c8ccdd6c435a7febbc67abe6adf8933dc66d8ee45c9eaab0407d9cc` |
| DESIGN | 主题七：SSE 流式解析（571） | [concepts/runtime.md](concepts/runtime.md) | `1be54de21be458db7747b59e311d839db0be44e5ee037af08b69e48b97314719` |
| DESIGN | 主题八：Session 与持久化（639） | [concepts/state-and-context.md](concepts/state-and-context.md) | `b2038726334ff89b3c056012be61d448e95d8655532cbc9b049c90019b57f5cb` |
| DESIGN | 主题九：SubAgent（子代理）（746） | [concepts/runtime.md](concepts/runtime.md) | `7a7072b8ed5123ad6cd6fca45d6b680cde4b1eb0fb822d3b7fdb4597d7cdb333` |
| DESIGN | 主题十：配置系统（799） | [reference/configuration.md](reference/configuration.md) | `e4b69c9063fea63ef15d1d8b7f350957e0cf9d3e7e843aea7cc5ef1984dee508` |
| DESIGN | 主题十一：并发模型（912） | [concepts/runtime.md](concepts/runtime.md) | `948f8b14a01e8aecfec28df6a54b7d68a3704445e42722ace6aeb9ab6a38d493` |
| DESIGN | 主题十二：系统提示词构建（967） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `e3a548b9c508001e4ac257162c5bf32ea633a242bde772283258f4bccdec5b6f` |
| DESIGN | 主题十三：Protocol 事件（1019） | [reference/protocols.md](reference/protocols.md) | `566b0ce9f21ca68b019204187aba98ddd24036405116781a47c05a87b9ac05fe` |
| DESIGN | 主题十四：不变式（Invariants）（1046） | [concepts/architecture.md](concepts/architecture.md) | `3fd545950839f4319d720406b7156b6463baf77b8730390cdeae830df15fab5d` |
| DESIGN | 主题十五：Rust 库 API 设计（1076） | [integration/rust.md](integration/rust.md) | `fe3a8379f58e22a006a8074d1fb5c059df5d90e514c1f5dc68d83a5ae50e9674` |
| DESIGN | 主题十六：多模态读图（v7 协议）（1140） | [concepts/images.md](concepts/images.md) | `c91a40f9dd3a2936edd0dbf179d4c4bd894be5991d4e6a45f2d239a4db1e6648` |
| DESIGN | 主题十七：LLM 有界自愈（1155） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `ecd32767d9628cb4fe319184502a08e40a82221cb2a939ab096a87d42f2daccf` |
| EMBEDDING | Rust 库嵌入（16） | [integration/rust.md](integration/rust.md) | `3614c6aece0feeff8c5d605d40582c8a24e35a2a8bf34f07e03a8dbda0285d66` |
| EMBEDDING | Python SDK（233） | [integration/python.md](integration/python.md) | `96ed20e215170161096c058a84061b36c4b994d8849810a1aa9fc5d41a8a921e` |
| EMBEDDING | Token 用量（313） | [reference/usage.md](reference/usage.md) | `dfd6974fc1d4ca9cad7ea56548ae658f17c7768734f15bedd95bfbe70baf55ac` |
| EMBEDDING | 性能基准（410） | [development/performance.md](development/performance.md) | `29a820ccd55dfd225496c14152074d8bf9dc51bcac19037c138465c8a2efcbe5` |
| EMBEDDING | 相关文档（457） | [start/overview.md](start/overview.md) | `2287c56a7b27c210563f909659d724e5fcdc2d9ccc4cd38c87756377b1254ee3` |
| PROTOCOL | Web 持久输入与快照协议（15） | [reference/http-api.md](reference/http-api.md) | `a44fe1e3dcc1f6ecf709bac1671b31fb0fb559cf166039eea035e5314c6df4e3` |
| PROTOCOL | Stream-JSON（`--print`）（26） | [reference/protocols.md](reference/protocols.md) | `b91783f26bf6e622305c441efbf74bf5dabb5cc616e9b597c6f52ec80e1e4402` |
| PROTOCOL | Agent JSONL（`--agent-jsonl`）（57） | [reference/protocols.md](reference/protocols.md) | `810c0c6f37661928caa27caf753268720db262b3a806847ed9f9b1ff9ac18bec` |
| PROTOCOL | 相关文档（161） | [start/overview.md](start/overview.md) | `55ce4cd368f88ba910bd9cd5c8896485472b6b0ff18c203823c74a964e754384` |
| server | 1. 概述（7） | [reference/http-api.md](reference/http-api.md) | `c14fe291a28029a86f96d4d52dc7f0608bac83806a24031c91d91945b571b2d0` |
| server | 2. 快速开始（27） | [guides/web.md](guides/web.md) | `7c6fca7183230de2730cd73fd046d5c9bfc2b65d4884b2fec8c9c72ba2e6f317` |
| server | 3. 配置（43） | [reference/configuration.md](reference/configuration.md) | `5c9e31b65211c3d4941867d131d4f78c7357635c1fefc4fdc0afd20643ed40b8` |
| server | 4. REST API（69） | [reference/http-api.md](reference/http-api.md) | `85623b1dc05c932d3faac34c784f82ac9d2068b93ef780582f0eaa1efd5f0916` |
| server | 5. SSE 事件（108） | [reference/http-api.md](reference/http-api.md) | `38708ff3b3398faa8802ef3b0098232b67e7ef05ae810701f3a0ab5b7f1182db` |
| server | 6. 静态资源与嵌入（164） | [development/web-and-site.md](development/web-and-site.md) | `1a824a0184c1fd5f35493b43b83c48c071a844b9c8b8a8e11d5e535565a39474` |
| server | 7. 生命周期与并发语义（171） | [reference/http-api.md](reference/http-api.md) | `d7674be2a110d7d9a1c2a6885fde9e91333cbd1b3ace0fb826bb7010fbfb181c` |
| server | 8. 部署注意（184） | [guides/security.md](guides/security.md) | `36d2467988a08d1d160dd9751797604b44d49377fa34e74f1c2d84b15ea26be4` |
| server | 9. 测试（194） | [development/web-and-site.md](development/web-and-site.md) | `174ce859d61404fba06fbcf55338b6eed699c650942912b6bab48ba95fd4a435` |
| tools | TUI 本地面板与工具状态（20） | [guides/terminal.md](guides/terminal.md) | `b2761a042f9e2cda9269e28731e59fee7df2a5a81bf4767122a13c09b5a209a6` |
| tools | Web 输入与展示元数据（33） | [reference/protocols.md](reference/protocols.md) | `797a23bbd1edd4a2409d529018b2c8c1631ac0ee7c3e20765a3a2bf3d0d9e41a` |
| tools | 执行模型（64） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `81af0a3d175e3c7b987a3c3a853f52e713e196e9c85c7075d0a686ab0fc33102` |
| tools | `Read`（194） | [reference/tools.md](reference/tools.md) | `f5367aebdf9de0116502ef9da2312ff21af69f091ce6dc61d990b5ea80b1d68a` |
| tools | `Write`（237） | [reference/tools.md](reference/tools.md) | `3171c7e29979bced54753d66d1a146bcb613e63978f220813ee96f79f8b3b44d` |
| tools | `Edit`（251） | [reference/tools.md](reference/tools.md) | `0f0af822f32c53adfb55050035b6507dddf8f45f747ccd00a87c479fa59d4329` |
| tools | `Bash`（306） | [reference/tools.md](reference/tools.md) | `9bbfb4ce81001dcae8589fbd8af42d02fba23af18138d82ca3ca71b269a22f07` |
| tools | `Python`（326） | [reference/tools.md](reference/tools.md) | `424734f4b964b3107b9c3cdc557bfcad331b3cfb7b6533b08f4532c12b9ba83b` |
| tools | `PythonSandbox`（345） | [reference/tools.md](reference/tools.md) | `565a6ecaa3711755c83acfda9f25d58a588fb158becadd47487a63ba2de74590` |
| tools | `Glob`（394） | [reference/tools.md](reference/tools.md) | `1900391092740e0904156d7e8e34a582ab269d6b6c9d3ad0b0edd0cc06264749` |
| tools | `Grep`（417） | [reference/tools.md](reference/tools.md) | `0ec3c03cf75423fc703acee60b55fd3b166ec84cedd44ee62b3b7d121f211b23` |
| tools | `TodoRead`（452） | [reference/tools.md](reference/tools.md) | `55a8598d2a30e9382f45f340832b5876515d09c6be2e7778b3c7a4e884c35d59` |
| tools | `TodoWrite`（461） | [reference/tools.md](reference/tools.md) | `858a0bb596e86a183828d031c753386bada848f80d0fde7150fe64004c00c954` |
| tools | `TodoAdvance`（477） | [reference/tools.md](reference/tools.md) | `1db74e53c8f5f22d904e1b8af82bad888d8fbef3c68b30542b024418bcb58f45` |
| tools | `PlanDraft`（508） | [reference/tools.md](reference/tools.md) | `e44c00b7a6d26741597dd115adac1d1c86ec73738a5cac13ca99c6891be9ade6` |
| tools | `PlanConfirm`（521） | [reference/tools.md](reference/tools.md) | `1bcb7587a13ada3a67f5d0b90254b6e39245425e7d1601ccc433cbc19611bba4` |
| tools | `PlanClear`（535） | [reference/tools.md](reference/tools.md) | `728c4276d480bca78b5a9297815e346980e8f95281b5ddc8a06eea57f44cca06` |
| tools | `SubAgent`（548） | [reference/tools.md](reference/tools.md) | `b34d342a7de45a30df75d037a9e1c8d79427403596e3588085e99d72c588a751` |
| tools | 工具事实权威表（Q11）（575） | [reference/tools.md](reference/tools.md) | `e38966045f66e1e8ccbb3ba53c30e90ae660a6c8ee4da50ef9f5a113992fbede` |
| 设计哲学-信号系统 | 一、概述（11） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `818a5767370177d210362152a43befd9e1819c04f676476e51eb1dc893d128ca` |
| 设计哲学-信号系统 | 二、设计思想（51） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `6721fcd5ffbc00bc06377beddb8283390084caeb1011942c61e8227d65b90d23` |
| 设计哲学-信号系统 | 三、信号采集（106） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `ad7e605a54cc6efbd0abe82d2661c80f31ebdf8a26756989648f071e1c788c78` |
| 设计哲学-信号系统 | 四、信念度计算（172） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `1bc410e4ef6340e4018bc03df31993926056baeb706ea9fbc89b6c306124f756` |
| 设计哲学-信号系统 | 五、决策与干预（229） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `ae58ec6497a853e3acb1176c1391f1609e4708d48a50ee5654169b5c5dd05af2` |
| 设计哲学-信号系统 | 六、信号链路完整路径（360） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `8f45854c5fd38ef13c8647794336a0711322c191f51c9b5a666ef57e465629af` |
| 设计哲学-信号系统 | 七、边界情况（410） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `0bb5af97c63bc0f4d2bb958563ba47592cb18f771146d2ee5c0023beec44c3ad` |
| 设计哲学-信号系统 | 八、组件接口（439） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `e3aafc5d85cc83e742c9652a77971e299b4dad44246b9c909bc148f6bb936db9` |
| 设计哲学-信号系统 | 九、信念度实时展示（481） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `c32c1b09031412d2164126e9717606be011e66b5c53dead752af27090ac158fa` |
| 设计哲学-信号系统 | 十、后续改进（526） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `41b9e6fa61d4f1b6ddf2ef6232ca793e2a423eabca6fd06cc2ab73b244fe5e85` |
| 设计哲学-信号系统 | 十一、相关文件（536） | [concepts/recovery-and-signals.md](concepts/recovery-and-signals.md) | `0c796395d2f3a94f08ed0bc9eafef686a1eb282b4ae17931bdcbabb136a893cd` |
| 设计哲学-工具能力与提示词解耦 | 一、问题、目标与边界（11） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `0cfdc578d5b58ea8f47b7a36a31b8dfbc81ffbff28c69cfcea5aa31025c46493` |
| 设计哲学-工具能力与提示词解耦 | 二、四层解析架构（69） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `df03089d634da24c4306e41761d3604f2228da4090d3745150718bead0b06996` |
| 设计哲学-工具能力与提示词解耦 | 三、语义能力与 Provider Binding（139） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `952babbadb0653d88acbaad30ba10319bdb54de375f1b557af661bc21c1c08a1` |
| 设计哲学-工具能力与提示词解耦 | 四、Workflow 与受约束前向求值（209） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `1479655ba6fda1e48ade07a8091c9f975a600a44a33c1bca621a31f2b59a361b` |
| 设计哲学-工具能力与提示词解耦 | 五、自由组合与组合爆炸（308） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `ba3864f73818866b3fcf4acefc94f0abcd2fe6ce37c59b1b7121e4b08efc5d0a` |
| 设计哲学-工具能力与提示词解耦 | 六、Prompt 所有权与按需加载（354） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `9051ee9905db6808ad1ecebf05c00f5d21eb63747130a79fbbca94b6502f7faa` |
| 设计哲学-工具能力与提示词解耦 | 七、运行时复用、安全与缓存（426） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `eab9d3471e51918d0c4b8b047ff8d80cc39c2dbc8a2a640cc8095a79299aaea5` |
| 设计哲学-工具能力与提示词解耦 | 八、核心不变式与验证原则（480） | [concepts/tools-and-capabilities.md](concepts/tools-and-capabilities.md) | `1272a04c5b26581a62e4a9064bd852729a2649ae9dc12dc672cb7b416a2641dd` |
| 设计哲学-多模态读图能力 | 1. 设计目标（9） | [concepts/images.md](concepts/images.md) | `cb0bf5f0f05c4d7f5f5fab03cf091af3b79fba60aad252b88947b5155e0b8069` |
| 设计哲学-多模态读图能力 | 2. 能力模型：会话怎么知道模型能看图（19） | [concepts/images.md](concepts/images.md) | `cfea407f582fc10c45bfc5900ca67f41e864edb4403a06c53502db0fb3b4c2c3` |
| 设计哲学-多模态读图能力 | 3. 读图流程（概念模型）（49） | [concepts/images.md](concepts/images.md) | `75e736abe1e8e34d01a8c4db5308115b335c38b0b585034bd555173e362b66a8` |
| 设计哲学-多模态读图能力 | 4. 单次消费生命周期（64） | [concepts/images.md](concepts/images.md) | `3fdca8072bb1cb8c6e893f840ddd3a5e08dbcc7a4d171dd4e4000450206f3660` |
| 设计哲学-多模态读图能力 | 5. 配额与预算：三层防线（79） | [concepts/images.md](concepts/images.md) | `e8044727d1f4ecfa4969165707fc225d81fc0e6fc60a995aff2dcfbcf9579f6a` |
| 设计哲学-多模态读图能力 | 6. 降级矩阵（行为表）（93） | [concepts/images.md](concepts/images.md) | `f7763b77763b8b0ccb77410fe07f93baae0da52f401075608235869a4090daea` |
| 设计哲学-多模态读图能力 | 7. 与系统其他部分的关系（108） | [concepts/images.md](concepts/images.md) | `15b8c5b7777f37d85bc1d7d89646c2e4cffbe43d5df27a5c0dfbd4ab43c0f101` |
| 设计哲学-多模态读图能力 | 8. 取舍：为什么不是别的方案（117） | [concepts/images.md](concepts/images.md) | `c25f3ece8f29bd100ea686e7c9d53b59368fd43c743a983e3cec0138ecf61cc8` |
| 设计哲学-多模态读图能力 | 9. 限制与未来演进（129） | [concepts/images.md](concepts/images.md) | `f01966e264c4d7553aa39e4eb37841009143b6f8671171af6e41afa9ac4bda9c` |
| TUI_OPTIMIZATION_ROADMAP | 定位（5） | [development/tui.md](development/tui.md) | `41e378b2fe6c7c225ac2f278164551badd019dce45683e5566ab5307f95a8527` |
| TUI_OPTIMIZATION_ROADMAP | 数据流（16） | [development/tui.md](development/tui.md) | `461fcba952393ff9b5927e93a004ea98c0a740800089236b24b058f157f0837c` |
| TUI_OPTIMIZATION_ROADMAP | 共用渲染（44） | [development/tui.md](development/tui.md) | `7f3dd39c6befee4c86493724795778d9a8b241e1e288f7fab1e26077352d6c0b` |
| TUI_OPTIMIZATION_ROADMAP | Full TUI（60） | [development/tui.md](development/tui.md) | `51be788c60eaf8e9af1cb3510e0e029ede6cf94093b68d84b5cb69cbd738e0e4` |
| TUI_OPTIMIZATION_ROADMAP | Inline TUI（74） | [development/tui.md](development/tui.md) | `19aa60e1285f4ea7f1186ac0e83bc2113781596e722e118f22b60db408f28f40` |
| TUI_OPTIMIZATION_ROADMAP | 不变式（91） | [development/tui.md](development/tui.md) | `5ec0669da24d078b5acc2296cb8de907daa183e35f0fc7d69aaa23af2658fadf` |
| TUI_OPTIMIZATION_ROADMAP | 验证（105） | [development/tui.md](development/tui.md) | `a8f9569ac4b96dca91c97c58bd9a367c99e66775501d5a9c5764d949d82ea54f` |
| TUI_OPTIMIZATION_ROADMAP | 性能验收与后续范围（129） | [development/tui.md](development/tui.md) | `9a4c27a210e0372ef07430aef20407ee87e1bff44c80c01a2a02a8c318af9dae` |
| TUI_PERFORMANCE-2026-10-04 | 测量范围（5） | [development/performance.md](development/performance.md) | `ab6ab226c909a4a8086a0f53e222de8d591505ce3248bfa598763970ad4a64da` |
| TUI_PERFORMANCE-2026-10-04 | 渲染结果（26） | [development/performance.md](development/performance.md) | `01d9f4d6a165f4a9d3f123a8dbf1550fd7a96c0ea2bcc0b8ef7baad1d6591344` |
| TUI_PERFORMANCE-2026-10-04 | 剩余可变尾部瓶颈（47） | [development/performance.md](development/performance.md) | `03d0d6a93f480fc95ec853608106acc207db248155e1ca63bd8df5fc463f8fc6` |
| TUI_PERFORMANCE-2026-10-04 | 终端交互（61） | [development/performance.md](development/performance.md) | `3a8c85b26b39a61d27f4188e2b12bd6e0f3e579ccf7f5c836cea911d01e19b8b` |
| TUI_PERFORMANCE-2026-10-04 | 正确性与构建（87） | [development/performance.md](development/performance.md) | `96f9b3c7356d6c782b4772f0b1a7280a22adb2b693597764ac68b4157f6f9891` |

实跑案例完整迁到 [examples/guided-env-parser.md](examples/guided-env-parser.md)。

事实核对依据：`agent/turn.rs` 严格收缩循环、`config.rs::validate_runtime_limits` 软额度、`sse/toolcall.rs` 严格 JSON 解析、`guard/storm.rs` 同类重复计数，以及 `session/plan.rs` / `session/todo.rs` 独立事务。参数表只保留在配置参考；重复操作说明用主题链接替代。

## 章节内二次拆分与归并

- Rust 库嵌入章节的自定义 backend、Retry/Usage、VFS 小节进入 integration/extensions；可靠流消费保留 integration/rust。
- 终端章节的 Ctrl+V 完整操作移到 guides/images，配置/工具页的旧粘贴锚点一起修正。
- 工具系统概要由完整 reference/tools 定义与 guides/plans-and-todos 操作说明替代；执行模型及资源/VFS 合同进入 concepts/tools-and-capabilities。
- 各压缩参数表并入 reference/configuration；guide 保留调优示例和成功/失败表现，concepts 保留完整收益/应急/期限机制。
- Python 包 README 的完整 AgentSession API 迁到 integration/python，配置字段迁到 reference/configuration，补齐当前 dataclass 字段索引。
- HTTP 页的 Web 客户端实现小节移到 development/web-and-site；HTTP/SSE 的权威字段与快照、Inbox 生命周期保留 reference/http-api。
- 包 README 只保留定位、安装/构建、最小示例与阅读入口；旧综述由 start/overview 的三条路径和清单取代。
