//! MCP（Model Context Protocol）stdio 服务器：AI 客户端按 JSON-RPC 2.0 每行一条消息调用。
//! 启动方式：`bitgalaxy_gifrecorder.exe mcp`
//! 会话流程：initialize → initialized（通知）→ tools/list → tools/call …
//!
//! 客户端配置示例（Claude Desktop / Trae 等）：
//! ```json
//! { "mcpServers": { "gifrecorder": {
//!     "command": "C:\\path\\to\\bitgalaxy_gifrecorder.exe", "args": ["mcp"] } } }
//! ```

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::automation;
use crate::json::{self, Json};
use crate::recorder::Config;

const SERVER_NAME: &str = "bitgalaxy-gifrecorder";

pub fn run() -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break }; // stdin 关闭
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg = match json::parse(line) {
            Ok(m) => m,
            Err(e) => {
                send(
                    &mut out,
                    &Json::obj(vec![
                        ("jsonrpc", Json::s("2.0")),
                        ("id", Json::Null),
                        ("error", err_obj(-32700, &format!("Parse error: {e}"))),
                    ]),
                );
                continue;
            }
        };
        // 只有请求带 id；通知（initialized / cancelled 等）直接忽略
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg.get("method").and_then(Json::as_str).unwrap_or("").to_string();

        let resp = match handle(&method, msg.get("params")) {
            Ok(result) => Json::obj(vec![
                ("jsonrpc", Json::s("2.0")),
                ("id", id),
                ("result", result),
            ]),
            Err(error) => Json::obj(vec![
                ("jsonrpc", Json::s("2.0")),
                ("id", id),
                ("error", error),
            ]),
        };
        send(&mut out, &resp);
    }
    0
}

fn send(out: &mut impl Write, v: &Json) {
    let _ = writeln!(out, "{}", v.to_string());
    let _ = out.flush();
}

fn err_obj(code: i64, msg: &str) -> Json {
    Json::obj(vec![("code", Json::i(code)), ("message", Json::s(msg))])
}

/// Ok = result；Err = JSON-RPC error 对象
fn handle(method: &str, params: Option<&Json>) -> Result<Json, Json> {
    match method {
        "initialize" => {
            // 回显客户端协议版本，兼容各代客户端
            let ver = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Json::as_str)
                .unwrap_or("2024-11-05")
                .to_string();
            Ok(Json::obj(vec![
                ("protocolVersion", Json::s(ver)),
                (
                    "capabilities",
                    Json::obj(vec![("tools", Json::obj(vec![("listChanged", Json::Bool(false))]))]),
                ),
                (
                    "serverInfo",
                    Json::obj(vec![
                        ("name", Json::s(SERVER_NAME)),
                        ("version", Json::s(env!("CARGO_PKG_VERSION"))),
                    ]),
                ),
            ]))
        }
        "ping" => Ok(Json::obj(vec![])),
        "tools/list" => Ok(Json::obj(vec![("tools", Json::arr(tools()))])),
        "tools/call" => {
            let name = params.and_then(|p| p.get("name")).and_then(Json::as_str).unwrap_or("");
            let args = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| Json::obj(vec![]));
            match call_tool(name, &args) {
                // 工具结果以 JSON 文本返回，AI 可直接解析
                Ok(v) => Ok(Json::obj(vec![(
                    "content",
                    Json::arr(vec![Json::obj(vec![
                        ("type", Json::s("text")),
                        ("text", Json::s(v.to_string())),
                    ])]),
                )])),
                Err(e) => Ok(Json::obj(vec![
                    (
                        "content",
                        Json::arr(vec![Json::obj(vec![
                            ("type", Json::s("text")),
                            ("text", Json::s(e)),
                        ])]),
                    ),
                    ("isError", Json::Bool(true)),
                ])),
            }
        }
        // 未声明能力的常见查询 → 空列表，兼容部分客户端
        "prompts/list" => Ok(Json::obj(vec![("prompts", Json::arr(vec![]))])),
        "resources/list" => Ok(Json::obj(vec![("resources", Json::arr(vec![]))])),
        _ => Err(err_obj(-32601, &format!("Method not found: {method}"))),
    }
}

// ============================================================
// 工具清单
// ============================================================

fn tools() -> Vec<Json> {
    vec![
        tool(
            "screens",
            "枚举显示器与虚拟屏坐标（物理像素）。返回 {virtual_screen:[x,y,w,h], monitors:[{x,y,w,h,primary,name}]}。\
             所有工具的 region 参数都使用该坐标系，多显示器时坐标可为负。",
            Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(vec![]))]),
        ),
        tool(
            "record_gif",
            "录制屏幕并导出 GIF 动画（无编辑器，一步到位）。duration_sec 为录制秒数；\
             region=[x,y,w,h] 省略时录制整个虚拟屏；fps 默认 15；scale_percent 默认 100；\
             speed>1 加速导出、<1 减速。建议 output 使用绝对路径。\
             需要先执行其他操作再停止的录制场景请改用 start_record/stop_record。",
            Json::obj(vec![
                ("type", Json::s("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("output", t("string", "输出 GIF 文件绝对路径")),
                        ("duration_sec", t("number", "录制时长（秒），0.5 ~ 600")),
                        ("region", region_schema()),
                        ("fps", t("integer", "帧率 1~50，默认 15")),
                        ("scale_percent", t("integer", "输出缩放 10~100，默认 100")),
                        ("speed", t("number", "导出变速倍率 0.1~10，默认 1（原速）")),
                    ]),
                ),
                (
                    "required",
                    Json::arr(vec![Json::s("output"), Json::s("duration_sec")]),
                ),
            ]),
        ),
        tool(
            "snapshot_gif",
            "截取当前屏幕单帧，保存为单帧 GIF（相当于截图）。建议 output 使用绝对路径。",
            Json::obj(vec![
                ("type", Json::s("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("output", t("string", "输出 GIF 文件绝对路径")),
                        ("region", region_schema()),
                        ("scale_percent", t("integer", "输出缩放 10~100，默认 100")),
                    ]),
                ),
                ("required", Json::arr(vec![Json::s("output")])),
            ]),
        ),
        tool(
            "start_record",
            "开始一条不限时长的录制（会话式）。随后可执行其他操作，最后用 stop_record 导出或 cancel_record 丢弃。\
             同一时刻只允许一条进行中的录制。",
            Json::obj(vec![
                ("type", Json::s("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("region", region_schema()),
                        ("fps", t("integer", "帧率 1~50，默认 15")),
                        ("scale_percent", t("integer", "输出缩放 10~100，默认 100")),
                    ]),
                ),
            ]),
        ),
        tool(
            "stop_record",
            "停止当前会话录制并导出 GIF，返回 {frames, duration_ms, size_bytes} 等统计。",
            Json::obj(vec![
                ("type", Json::s("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("output", t("string", "输出 GIF 文件绝对路径")),
                        ("speed", t("number", "导出变速倍率 0.1~10，默认 1（原速）")),
                    ]),
                ),
                ("required", Json::arr(vec![Json::s("output")])),
            ]),
        ),
        tool(
            "cancel_record",
            "丢弃当前会话录制（自动清理临时文件）。",
            Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(vec![]))]),
        ),
        tool(
            "get_status",
            "查询当前录制状态：{recording, elapsed_ms, frames}。",
            Json::obj(vec![("type", Json::s("object")), ("properties", Json::obj(vec![]))]),
        ),
    ]
}

fn tool(name: &str, desc: &str, schema: Json) -> Json {
    Json::obj(vec![
        ("name", Json::s(name)),
        ("description", Json::s(desc)),
        ("inputSchema", schema),
    ])
}

fn t(kind: &str, desc: &str) -> Json {
    Json::obj(vec![("type", Json::s(kind)), ("description", Json::s(desc))])
}

fn region_schema() -> Json {
    Json::obj(vec![
        ("type", Json::s("array")),
        ("items", Json::obj(vec![("type", Json::s("integer"))])),
        ("minItems", Json::i(4)),
        ("maxItems", Json::i(4)),
        (
            "description",
            Json::s("录制区域 [x,y,w,h]，虚拟屏绝对物理像素坐标（先用 screens 工具查询）；省略=整个虚拟屏"),
        ),
    ])
}

// ============================================================
// 工具实现
// ============================================================

fn call_tool(name: &str, a: &Json) -> Result<Json, String> {
    match name {
        "screens" => Ok(automation::screens_json()),
        "record_gif" => {
            let out = req_path(a)?;
            let dur = a.get("duration_sec").and_then(Json::as_f64).unwrap_or(5.0);
            if !(0.5..=600.0).contains(&dur) {
                return Err(format!("duration_sec 需在 0.5 ~ 600（当前 {dur}）"));
            }
            let cfg = config_from(a)?;
            let speed = a.get("speed").and_then(Json::as_f64).map(|s| s.clamp(0.1, 10.0));
            automation::record_to_gif(cfg, Duration::from_secs_f64(dur), &out, speed, |_, _| {})
                .map(|s| s.to_json())
        }
        "snapshot_gif" => {
            let out = req_path(a)?;
            let cfg = config_from(a)?;
            automation::snapshot_gif(cfg.region, cfg.scale_percent, &out).map(|s| s.to_json())
        }
        "start_record" => {
            let cfg = config_from(a)?;
            automation::start_rec(cfg)
                .map(|_| Json::obj(vec![("ok", Json::Bool(true)), ("started", Json::Bool(true))]))
        }
        "stop_record" => {
            let out = req_path(a)?;
            let speed = a.get("speed").and_then(Json::as_f64).map(|s| s.clamp(0.1, 10.0));
            automation::stop_rec(&out, speed).map(|s| s.to_json())
        }
        "cancel_record" => automation::cancel_rec()
            .map(|_| Json::obj(vec![("ok", Json::Bool(true)), ("cancelled", Json::Bool(true))])),
        "get_status" => Ok(automation::status_json()),
        _ => Err(format!("未知工具: {name}")),
    }
}

fn req_path(a: &Json) -> Result<PathBuf, String> {
    let p = a
        .get("output")
        .and_then(Json::as_str)
        .ok_or("缺少必需参数 output（输出 GIF 的绝对路径）")?;
    if p.trim().is_empty() {
        return Err("output 不能为空".into());
    }
    Ok(PathBuf::from(p))
}

fn config_from(a: &Json) -> Result<Config, String> {
    // 显式 null 视同省略
    let region = match a.get("region").filter(|v| !v.is_null()) {
        Some(v) => {
            let arr = v.as_arr().ok_or("region 需为 [x,y,w,h] 整数数组")?;
            if arr.len() != 4 {
                return Err(format!("region 需为 [x,y,w,h]，收到 {} 个元素", arr.len()));
            }
            let mut n = [0i32; 4];
            for (i, x) in arr.iter().enumerate() {
                let f = x.as_f64().ok_or("region 需为整数数组")?;
                n[i] = f.round() as i64 as i32;
            }
            Some((n[0], n[1], n[2], n[3]))
        }
        None => None,
    };
    let fps = a.get("fps").and_then(Json::as_f64).map(|v| v.round().clamp(1.0, 50.0) as u32);
    let scale = a
        .get("scale_percent")
        .and_then(Json::as_f64)
        .map(|v| v.round().clamp(10.0, 100.0) as u32);
    automation::build_config(region, fps, scale)
}
