
Win_Little_Tools


- pid_io_monitor (rust)
    - win10 小工具 ，监视进程(pid) 的 磁盘I/O行为，会打印目录访问情况(e.g. 某程序访问E盘 → 打印`E:\test\` )
    - 支持常见的软件进程(浏览器，记事本，excel 等)，包括服务进程 `Service Host: Winodws Event log` （ win 的系统进程 除外）
    - CLI 用法:  获取目标 pid(如`1234`) → 管理员打开cmd/pwsh → `.\pid_io_monitor.exe 1234` → 显示 `ETW 会话已启动，...`
    - 开发信息: rust crate 主要是 `ferrisetw` 和 `windows` 

- jumpbar (rust)
    - win10 存开文件对话框时，快切回目录的小工具，，详细说: 在文件的「打开 / 保存 / 另存为 / 选择文件夹」对话框里按一下 Ctrl+G，就把对话框直接切到你刚才在资源管理器里打开的那个文件夹 （若没有这个，可能要在目标点很久，才定位到当前需要的目录）
    - 翻译自 C# 项目(v1.0.9),原作者 `大军哥`,（原项目依赖 .net runtime）
    - rust 版 不用 GUI，无头服务模式，配置 `settings.ini`（主要是热键 + 默认文件夹） → 放在 .exe 同目录 → 双击运行。要关闭 可以从任务管理器，要开机自启 放快捷方式到 Startup 文件夹  （settings.ini 写法，可以参考  [模板文件](./jumpbar/settings.ini)）


















