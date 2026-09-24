fn main() {
    tauri_build::build();

    // 给测试目标也嵌一份 Windows 清单。
    //
    // 为什么需要：`tauri-build` 会给应用二进制写清单（含 comctl32 v6 依赖声明），
    // 但测试二进制不会走那条路。而测试链接了 `rfd`（经 dialog 插件间接引入），
    // 它调用 `TaskDialogIndirect` —— 这个函数只在 comctl32 v6 里存在。
    // 没有 v6 声明时加载器绑定到 comctl32 v5，进程在加载阶段就报
    // `STATUS_ENTRYPOINT_NOT_FOUND`（0xC0000139），表现是"测试毫无输出地失败"。
    //
    // 验证方式（本机实测）：`findstr /m /c:"TaskDialogIndirect" <bin>` 在两个二进制里都能查到，
    // 但只有应用二进制有 v6 清单。
    #[cfg(windows)]
    {
        let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR 缺失");
        let manifest_path = std::path::Path::new(&out_dir).join("screenlite-tests.manifest");
        std::fs::write(&manifest_path, manifest).expect("写入测试清单失败");

        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
            manifest_path.display()
        );
    }
}
