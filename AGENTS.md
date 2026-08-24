## 注意

- 当前项目是rust项目，修改完代码后使用cargo fmt格式化项目，然后使用cargo clippy --all-targets 来替代 cargo check 和 cargo build检查是否有语法错误
- 用户提到git 提交的时候，必须使用-S 参数，阅读gitmoji技能，查看当前变更，写好提交的信息到 /tmp/gitcommit 文件里，使用-F参数使用这个文件，提交并且推送
- 使用 cargo add 添加依赖，而不是直接编辑Cargo.toml文件
