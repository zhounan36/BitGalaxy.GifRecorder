//! MCP 一键安装 / 卸载：把本程序（`exe mcp` stdio 服务器）注册进本机 AI 客户端的配置文件，
//! 或从中移除。主窗口「MCP · AI 集成」面板调用；只改写本程序自己的条目，其余配置原样保留。
//!
//! 支持的客户端与配置位置：
//! - Trae / Trae CN   %APPDATA%\Trae CN\User\mcp.json（mcpServers）
//! - Claude Desktop   %APPDATA%\Claude\claude_desktop_config.json（mcpServers）
//! - Cursor           %USERPROFILE%\.cursor\mcp.json（mcpServers）
//! - Windsurf         %USERPROFILE%\.codeium\windsurf\mcp_config.json（mcpServers）
//! - VS Code          %USERPROFILE%\.vscode\mcp.json（servers，stdio）

use std::path::PathBuf;

use crate::json::{self, Json};

/// 各客户端配置里的服务器条目名
pub const SERVER_KEY: &str = "gifrecorder";

/// 一个客户端的注册位置描述
struct ClientSpec {
    name: &'static str,
    /// MCP 配置文件
    config: PathBuf,
    /// 客户端用户目录（存在 = 该客户端已安装，才允许写入配置）
    home: PathBuf,
    /// 配置文件里的顶层节名（"mcpServers" / "servers"）
    section: &'static str,
    /// VS Code 风格条目需要额外的 "type":"stdio"
    vscode_style: bool,
}

fn roaming() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default()
}

fn profile() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
}

/// Trae 有国际版 / 国内版两个用户目录，取已安装的那个
fn trae_spec() -> Option<ClientSpec> {
    let r = roaming();
    for dir in ["Trae CN", "Trae"] {
        let home = r.join(dir);
        if home.is_dir() {
            return Some(ClientSpec {
                name: "Trae",
                config: home.join("User").join("mcp.json"),
                home,
                section: "mcpServers",
                vscode_style: false,
            });
        }
    }
    None
}

fn specs() -> Vec<ClientSpec> {
    let mut v = Vec::new();
    if let Some(t) = trae_spec() {
        v.push(t);
    }
    v.push(ClientSpec {
        name: "Claude Desktop",
        config: roaming().join("Claude").join("claude_desktop_config.json"),
        home: roaming().join("Claude"),
        section: "mcpServers",
        vscode_style: false,
    });
    v.push(ClientSpec {
        name: "Cursor",
        config: profile().join(".cursor").join("mcp.json"),
        home: profile().join(".cursor"),
        section: "mcpServers",
        vscode_style: false,
    });
    v.push(ClientSpec {
        name: "Windsurf",
        config: profile().join(".codeium").join("windsurf").join("mcp_config.json"),
        home: profile().join(".codeium"),
        section: "mcpServers",
        vscode_style: false,
    });
    v.push(ClientSpec {
        name: "VS Code",
        config: profile().join(".vscode").join("mcp.json"),
        home: profile().join(".vscode"),
        section: "servers",
        vscode_style: true,
    });
    v
}

/// 单个客户端的检测结果（供 UI 显示安装状态）
pub struct ClientState {
    pub name: &'static str,
    /// 客户端已安装（用户目录存在）
    pub detected: bool,
    /// 配置中已注册本程序
    pub installed: bool,
}

/// 扫描本机全部受支持客户端
pub fn scan() -> Vec<ClientState> {
    specs()
        .into_iter()
        .map(|c| ClientState {
            name: c.name,
            detected: c.home.is_dir(),
            installed: match read_config(&c.config) {
                Ok(root) => root
                    .get(c.section)
                    .and_then(|s| s.get(SERVER_KEY))
                    .is_some(),
                Err(_) => false,
            },
        })
        .collect()
}

/// 是否有任意客户端已注册本程序
pub fn any_installed() -> bool {
    scan().into_iter().any(|c| c.installed)
}

/// UI 状态行文本
pub fn status_text() -> String {
    let st = scan();
    let parts: Vec<String> = st
        .iter()
        .filter(|c| c.detected)
        .map(|c| {
            if c.installed {
                format!("{} ✓已注册", c.name)
            } else {
                format!("{} ✗未注册", c.name)
            }
        })
        .collect();
    if parts.is_empty() {
        "未检测到已安装的 AI 客户端（支持 Trae / Claude Desktop / Cursor / Windsurf / VS Code）".into()
    } else {
        format!("已检测到：{}（重启客户端后生效）", parts.join(" · "))
    }
}

/// 一键安装 / 卸载结果：成功处理的客户端 + 失败原因
pub struct ChangeResult {
    pub ok: Vec<String>,
    pub failed: Vec<String>,
}

impl ChangeResult {
    /// 安装结果摘要（用于消息框 / 托盘气泡）
    pub fn install_message(&self) -> String {
        if self.ok.is_empty() {
            self.summary("安装")
        } else {
            format!("{}\n重启 AI 客户端后即可在 MCP 列表看到 gifrecorder。", self.summary("注册到"))
        }
    }

    /// 卸载结果摘要
    pub fn uninstall_message(&self) -> String {
        self.summary("从配置移除")
    }

    fn summary(&self, verb: &str) -> String {
        let mut s = String::new();
        if !self.ok.is_empty() {
            s.push_str(&format!("已{}：{}\n", verb, self.ok.join("、")));
        }
        if !self.failed.is_empty() {
            s.push_str(&format!("失败：{}", self.failed.join("\n")));
        }
        s.trim_end().to_string()
    }
}

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "bitgalaxy_gifrecorder.exe".into())
}

/// 读取配置文件：文件不存在 → 空对象；解析失败 → Err（绝不覆盖用户配置）
fn read_config(path: &PathBuf) -> Result<Json, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let text = text.trim_start_matches('\u{FEFF}'); // 去掉可能的 BOM
            json::parse(text).map_err(|e| format!("配置解析失败（{}）：{e}", path.display()))
        }
        Err(_) => Ok(Json::obj(vec![])),
    }
}

fn write_config(path: &PathBuf, root: &Json) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, format!("{}\n", root.to_string_pretty()))
        .map_err(|e| format!("写入失败（{}）：{e}", path.display()))
}

/// 修改某个客户端配置：add=true 写入/更新条目，add=false 移除条目
fn modify(c: &ClientSpec, add: bool) -> Result<bool, String> {
    let mut root = read_config(&c.config)?;
    let Json::Obj(pairs) = &mut root else {
        return Err(format!("配置顶层不是对象（{}）", c.config.display()));
    };

    // 定位/创建顶层节
    if !pairs.iter().any(|(k, _)| k == c.section) {
        if !add {
            return Ok(false); // 没有该节 → 无需改动
        }
        pairs.push((c.section.to_string(), Json::obj(vec![])));
    }
    let section = pairs
        .iter_mut()
        .find(|(k, _)| k == c.section)
        .map(|(_, v)| v)
        .expect("section just ensured");
    let Json::Obj(items) = section else {
        return Err(format!("“{}”节不是对象（{}）", c.section, c.config.display()));
    };

    let idx = items.iter().position(|(k, _)| k == SERVER_KEY);
    if add {
        let mut entry = vec![
            ("command".to_string(), Json::s(exe_path())),
            ("args".to_string(), Json::arr(vec![Json::s("mcp")])),
        ];
        if c.vscode_style {
            entry.insert(0, ("type".to_string(), Json::s("stdio")));
        } else {
            entry.push(("env".to_string(), Json::obj(vec![])));
        }
        match idx {
            Some(i) => items[i].1 = Json::Obj(entry), // 更新（如 exe 挪了位置）
            None => items.push((SERVER_KEY.to_string(), Json::Obj(entry))),
        }
        write_config(&c.config, &root)?;
        Ok(true)
    } else {
        match idx {
            Some(i) => {
                items.remove(i);
                // 配置只剩一个空节（文件是安装时新建的）→ 直接删文件
                let only_empty_section = match &root {
                    Json::Obj(top) => {
                        top.len() == 1
                            && top[0].0 == c.section
                            && matches!(&top[0].1, Json::Obj(v) if v.is_empty())
                    }
                    _ => false,
                };
                if only_empty_section {
                    let _ = std::fs::remove_file(&c.config);
                } else {
                    write_config(&c.config, &root)?;
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// 一键安装：写入所有已安装客户端的配置
pub fn install() -> ChangeResult {
    let mut r = ChangeResult { ok: vec![], failed: vec![] };
    for c in specs() {
        if !c.home.is_dir() {
            continue; // 客户端未安装，跳过
        }
        match modify(&c, true) {
            Ok(_) => r.ok.push(c.name.to_string()),
            Err(e) => r.failed.push(format!("{}：{e}", c.name)),
        }
    }
    if r.ok.is_empty() && r.failed.is_empty() {
        r.failed.push("未检测到已安装的 AI 客户端".into());
    }
    r
}

/// 卸载：从所有配置了本程序的客户端中移除条目
pub fn uninstall() -> ChangeResult {
    let mut r = ChangeResult { ok: vec![], failed: vec![] };
    for c in specs() {
        match modify(&c, false) {
            Ok(true) => r.ok.push(c.name.to_string()),
            Ok(false) => {}
            Err(e) => r.failed.push(format!("{}：{e}", c.name)),
        }
    }
    if r.ok.is_empty() && r.failed.is_empty() {
        r.failed.push("没有客户端注册了本程序".into());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_runs() {
        // 只验证扫描不崩溃、字段自洽
        for c in scan() {
            if !c.detected {
                assert!(!c.installed);
            }
        }
    }
}
