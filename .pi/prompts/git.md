---
description: Review staged git changes and commit
---

1. 检查本次有哪些变更 
2. 阅读gitmoji技能，写好相关的信息到 /tmp/gitcommit 文件
3. git add . && git status, 确保所有文件都已经追踪
4. git commit -S -F /tmp/gitcommit && git push
