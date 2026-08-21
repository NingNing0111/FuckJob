# Delivery

状态：Partial
- 步骤 review 最新一轮评审仍有 blocking：[qa-engineer] Bugfix 缺少修复前红灯证据：`output/boss-chat-tab-timeout-fix.md:13-18` 只描述修复后的行为测试，共享黑板也只记录修复后 `reply_unread` 测试 9 passed、0 failed；没有真实命令输出证明其中至少一个回归用例在旧的无条件导航实现上失败。按 bugfix 红→绿要求，现有证据不足以支撑可合并，需要保留旧实现或修复前提交上的同一目标测试失败输出，以及当前实现上的通过输出。
- 步骤 review 最新一轮评审仍有 blocking：[qa-engineer] 关键错误分支“首次等待失败后，恢复导航本身失败”没有行为测试覆盖。实现位于 `src-tauri/src/rpa/boss/handler/reply_unread.rs:218-221`，但现有四个编排测试 `src-tauri/src/rpa/boss/handler/reply_unread.rs:743-785` 只覆盖就绪复用、首次等待成功、恢复等待成功和两次等待超时；没有给 `navigate_results` 注入恢复导航错误并断言恰好两次导航、仅一次等待，以及错误同时保留首次等待和恢复导航上下文。这是已批准诊断合同 `output/boss-chat-tab-timeout-diagnosis.md:28-30` 所要求的最终失败上下文路径，属于关键失败路径测试断裂。
- 步骤 review 最新一轮评审仍有 blocking：[devops-engineer] 缺少真实 BOSS 运行环境中的启动与行为响应证据，现有证据只能证明编排单元测试和静态检查通过，不能证明周期任务在标签超时场景下已恢复稳定运行，因此不足以支撑本修复为 release-ready。`output/boss-chat-tab-timeout-fix.md:28` 明确承认线上页面与风控响应无法由本地单元测试完整复现；至少需要核对修复版本的运行日志，证明就绪页被复用、首次失败时仅恢复一次，并且后续能够进入未读列表处理或明确终止。
- 步骤 review 最新一轮评审仍有 blocking：[devops-engineer] `output/boss-chat-tab-timeout-fix.md:3-8,28` 将重复冷导航直接定性为“根因”和“已确认”，但文档同时承认没有真实线上复现；当前日志只能支持该因素与故障相符，尚不能排除登录状态、风控页面、标签 DOM 陈旧或接口加载异常。该完成措辞超出现有证据，存在发布结论失真的风险。

## 交付范围
- 目标：定位 BOSS 周期间歇自动回复反复等待会话分类标签超时的根因，实施稳健修复并验证
- 路由：class=debug · kind=bugfix · depth=standard
- 计划 boss-chat-tab-timeout：3/3 done · 0 blocked · 0 未结算
  - [done] diagnose 追踪沟通页面导航、分类标签等待与周期重试链路，形成根因诊断（backend-engineer，验收 source-present）
  - [done] fix 修复分类标签就绪判定与异常恢复逻辑并补充回归测试（backend-engineer，验收 build-test）
  - [done] review 评审故障修复的正确性、回归风险与可观测性（qa-engineer，验收 review-clean）

## 关键文件
- src-tauri/src/rpa/boss/handler/chat_list.rs（主会话声明的改动）
- src-tauri/src/rpa/boss/handler/reply_unread.rs（主会话声明的改动）
- output/boss-chat-tab-timeout-fix.md（主会话声明的改动）
- output/boss-chat-tab-timeout-review.md（主会话声明的改动）

## 验证
- [diagnose] source-present → pass
- [fix] source-present → pass
- [fix] `pnpm run build` → pass exit=0
- [fix] `pnpm run test` → pass exit=0
- [fix] lint → skipped
- [fix] typecheck → skipped
- [review] review-clean → skipped
- 合同：N/A —— 本次改动不涉及前后端接口合同
- 治理/安全：review：安全席无 blocking（仅评审意见，未替代自动化扫描）
- 运行：not verified —— 未启动服务做真实运行探测
- 部署：not deployed —— 未获部署授权，也未执行任何部署动作

## 未完成或风险
- 步骤 review 最新一轮评审仍有 blocking：[qa-engineer] Bugfix 缺少修复前红灯证据：`output/boss-chat-tab-timeout-fix.md:13-18` 只描述修复后的行为测试，共享黑板也只记录修复后 `reply_unread` 测试 9 passed、0 failed；没有真实命令输出证明其中至少一个回归用例在旧的无条件导航实现上失败。按 bugfix 红→绿要求，现有证据不足以支撑可合并，需要保留旧实现或修复前提交上的同一目标测试失败输出，以及当前实现上的通过输出。
- 步骤 review 最新一轮评审仍有 blocking：[qa-engineer] 关键错误分支“首次等待失败后，恢复导航本身失败”没有行为测试覆盖。实现位于 `src-tauri/src/rpa/boss/handler/reply_unread.rs:218-221`，但现有四个编排测试 `src-tauri/src/rpa/boss/handler/reply_unread.rs:743-785` 只覆盖就绪复用、首次等待成功、恢复等待成功和两次等待超时；没有给 `navigate_results` 注入恢复导航错误并断言恰好两次导航、仅一次等待，以及错误同时保留首次等待和恢复导航上下文。这是已批准诊断合同 `output/boss-chat-tab-timeout-diagnosis.md:28-30` 所要求的最终失败上下文路径，属于关键失败路径测试断裂。
- 步骤 review 最新一轮评审仍有 blocking：[devops-engineer] 缺少真实 BOSS 运行环境中的启动与行为响应证据，现有证据只能证明编排单元测试和静态检查通过，不能证明周期任务在标签超时场景下已恢复稳定运行，因此不足以支撑本修复为 release-ready。`output/boss-chat-tab-timeout-fix.md:28` 明确承认线上页面与风控响应无法由本地单元测试完整复现；至少需要核对修复版本的运行日志，证明就绪页被复用、首次失败时仅恢复一次，并且后续能够进入未读列表处理或明确终止。
- 步骤 review 最新一轮评审仍有 blocking：[devops-engineer] `output/boss-chat-tab-timeout-fix.md:3-8,28` 将重复冷导航直接定性为“根因”和“已确认”，但文档同时承认没有真实线上复现；当前日志只能支持该因素与故障相符，尚不能排除登录状态、风控页面、标签 DOM 陈旧或接口加载异常。该完成措辞超出现有证据，存在发布结论失真的风险。
- 未验证：fix / lint：skipped —— 未探测到该项目的对应命令，也没有在 .pi/dev/config.json 中配置
- 未验证：fix / typecheck：skipped —— 未探测到该项目的对应命令，也没有在 .pi/dev/config.json 中配置
- 未验证：review / review-clean：skipped —— 只读评审由 dev_review 调度，不在机械验收中判定

## 证据
- `.pi/dev/plan.json`：计划 DAG 与每步状态（可恢复）
- `.pi/dev/route.json`：本次路由定级与理由
- `.pi/dev/ledger.jsonl`：append-only 审计账本（路由 / 验收 / 评审 / 确认门 / 交付）
- 验收报告 4 份，最近一次 review @ 2026-08-21T06:09:15.477Z
- `.pi/dev/evidence/`：命令完整输出（报告中的 output 已截断）
- 评审报告 2 份，含各席位 accepts / blocking / advisory / evidence 原文
- `.pi/dev/blackboard.md`：当前黑板（合同、实际状态、finding、待确认项）

## 恢复/继续
- 计划已全部结算；继续新目标请先调用 dev_route 重新定级，不要沿用本次路由