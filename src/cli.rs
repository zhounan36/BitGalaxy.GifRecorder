//! 命令行接口（供 AI/脚本调用）：record / snapshot / screens / mcp。
//! 结果 JSON 输出到 stdout，进度到 stderr；退出码 0=成功 1=失败 2=用法错误。

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::automation;
use crate::json::Json;

const USAGE: &str = "\
BitGalaxy GIF 录屏 —— 命令行 / AI 接口（无参数启动则进入图形界面）

用法:
  bitgalaxy_gifrecorder record   -o <out.gif> [-d <秒>] [-r <x,y,w,h>] [-f <fps>] [-s <百分比>] [--speed <倍率>]
  bitgalaxy_gifrecorder snapshot -o <out.gif> [-r <x,y,w,h>] [-s <百分比>]
  bitgalaxy_gifrecorder screens
  bitgalaxy_gifrecorder mcp
  bitgalaxy_gifrecorder mcp-status
  bitgalaxy_gifrecorder mcp-install
  bitgalaxy_gifrecorder mcp-uninstall
  bitgalaxy_gifrecorder help

命令:
  record    录制屏幕并直接导出 GIF（跳过编辑器）
  snapshot  截取当前屏幕单帧，保存为单帧 GIF
  screens   列出显示器与虚拟屏坐标（物理像素，JSON）
  mcp       作为 MCP（Model Context Protocol）stdio 服务器运行，供 AI 客户端调用
  mcp-status    查询本机 AI 客户端的 MCP 注册状态（JSON）
  mcp-install   把本程序一键注册进已检测到的 AI 客户端（Trae / Claude Desktop / Cursor / Windsurf / VS Code）
  mcp-uninstall 从所有 AI 客户端配置中移除本程序的 MCP 注册
  help      显示本帮助

选项:
  -o, --output <路径>     输出 GIF 文件（建议绝对路径）
  -d, --duration <秒>     录制时长，默认 5（0.5 ~ 600）
  -r, --region <x,y,w,h>  录制区域，虚拟屏绝对物理像素坐标（先用 screens 查询），默认整个虚拟屏
  -f, --fps <n>           帧率，默认 15（1 ~ 50）
  -s, --scale <百分比>     输出缩放，默认 100（10 ~ 100）
      --speed <倍率>       导出变速：>1 加速、<1 减速（0.1 ~ 10）

所有结果以 JSON 打印到 stdout（ok=true/false），进度打印到 stderr。";

pub fn run(args: &[String]) -> i32 {
    let cmd = args[0].as_str();
    match cmd {
        "record" => cmd_record(&args[1..]),
        "snapshot" => cmd_snapshot(&args[1..]),
        "screens" => {
            print_json(&automation::screens_json());
            0
        }
        "mcp" => crate::mcp::run(),
        "mcp-status" => {
            use crate::json::Json;
            let clients: Vec<Json> = crate::mcp_install::scan()
                .into_iter()
                .map(|c| {
                    Json::obj(vec![
                        ("client", Json::s(c.name)),
                        ("detected", Json::Bool(c.detected)),
                        ("installed", Json::Bool(c.installed)),
                    ])
                })
                .collect();
            print_json(&Json::obj(vec![
                ("ok", Json::Bool(true)),
                ("clients", Json::arr(clients)),
            ]));
            0
        }
        "mcp-install" => {
            let r = crate::mcp_install::install();
            print_json(&result_json(&r));
            if r.ok.is_empty() {
                1
            } else {
                0
            }
        }
        "mcp-uninstall" => {
            let r = crate::mcp_install::uninstall();
            print_json(&result_json(&r));
            if r.ok.is_empty() {
                1
            } else {
                0
            }
        }
        "help" | "-h" | "--help" | "/?" => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("未知命令: {cmd}");
            println!("{USAGE}");
            2
        }
    }
}

// ============================================================
// record / snapshot
// ============================================================

struct Opts {
    output: Option<String>,
    duration: Option<f64>,
    region: Option<(i32, i32, i32, i32)>,
    fps: Option<u32>,
    scale: Option<u32>,
    speed: Option<f64>,
}

fn cmd_record(rest: &[String]) -> i32 {
    let o = match parse_opts(rest, true) {
        Ok(o) => o,
        Err(e) => return usage_fail(&e),
    };
    let Some(out) = o.output.clone() else {
        return usage_fail("record 需要 -o <输出路径>");
    };
    let out = PathBuf::from(&out);
    let dur = o.duration.unwrap_or(5.0);
    if !(0.5..=600.0).contains(&dur) {
        return usage_fail(&format!("duration 需在 0.5 ~ 600 秒之间（当前 {dur}）"));
    }
    if let Some(sp) = o.speed {
        if !(0.1..=10.0).contains(&sp) {
            return usage_fail("speed 需在 0.1 ~ 10 之间");
        }
    }
    let cfg = match automation::build_config(o.region, o.fps, o.scale) {
        Ok(c) => c,
        Err(e) => return usage_fail(&e),
    };

    let stats = automation::record_to_gif(cfg, Duration::from_secs_f64(dur), &out, o.speed, |el, total| {
        eprint!("\r录制中 {:.1}/{:.1}s …", el as f64 / 1000.0, total as f64 / 1000.0);
    });
    eprintln!();
    match stats {
        Ok(s) => {
            print_json(&s.to_json());
            0
        }
        Err(e) => fail(&e),
    }
}

fn cmd_snapshot(rest: &[String]) -> i32 {
    let o = match parse_opts(rest, false) {
        Ok(o) => o,
        Err(e) => return usage_fail(&e),
    };
    let Some(out) = o.output.clone() else {
        return usage_fail("snapshot 需要 -o <输出路径>");
    };
    let cfg = match automation::build_config(o.region, o.fps, o.scale) {
        Ok(c) => c,
        Err(e) => return usage_fail(&e),
    };
    match automation::snapshot_gif(cfg.region, cfg.scale_percent, &PathBuf::from(&out)) {
        Ok(s) => {
            print_json(&s.to_json());
            0
        }
        Err(e) => fail(&e),
    }
}

// ============================================================
// 参数解析
// ============================================================

fn parse_opts(rest: &[String], allow_duration: bool) -> Result<Opts, String> {
    let mut o = Opts {
        output: None,
        duration: None,
        region: None,
        fps: None,
        scale: None,
        speed: None,
    };
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "-o" | "--output" => o.output = Some(next_val(rest, &mut i, a)?.to_string()),
            "-d" | "--duration" if allow_duration => {
                let v = next_val(rest, &mut i, a)?;
                o.duration = Some(v.parse().map_err(|_| format!("duration 需为数字（收到 '{v}'）"))?);
            }
            "-r" | "--region" => {
                let v = next_val(rest, &mut i, a)?;
                o.region = Some(parse_region(&v)?);
            }
            "-f" | "--fps" => {
                let v = next_val(rest, &mut i, a)?;
                o.fps = Some(v.parse().map_err(|_| format!("fps 需为整数（收到 '{v}'）"))?);
            }
            "-s" | "--scale" => {
                let v = next_val(rest, &mut i, a)?;
                o.scale = Some(v.parse().map_err(|_| format!("scale 需为整数（收到 '{v}'）"))?);
            }
            "--speed" => {
                let v = next_val(rest, &mut i, a)?;
                o.speed = Some(v.parse().map_err(|_| format!("speed 需为数字（收到 '{v}'）"))?);
            }
            _ => return Err(format!("未知选项: {a}")),
        }
        i += 1;
    }
    Ok(o)
}

fn next_val<'a>(rest: &'a [String], i: &mut usize, flag: &str) -> Result<&'a str, String> {
    *i += 1;
    rest.get(*i)
        .map(|s| s.as_str())
        .ok_or_else(|| format!("缺少 {flag} 的值"))
}

fn parse_region(s: &str) -> Result<(i32, i32, i32, i32), String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(format!("region 格式应为 x,y,w,h（收到 '{s}'）"));
    }
    let n: Result<Vec<i32>, _> = parts.iter().map(|p| p.trim().parse::<i32>()).collect();
    let n = n.map_err(|_| format!("region 含非整数: '{s}'"))?;
    Ok((n[0], n[1], n[2], n[3]))
}

// ============================================================
// 输出
// ============================================================

fn print_json(v: &Json) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", v.to_string());
    let _ = out.flush();
}

/// mcp-install / mcp-uninstall 的结果 JSON
fn result_json(r: &crate::mcp_install::ChangeResult) -> Json {
    Json::obj(vec![
        ("ok", Json::Bool(!r.ok.is_empty())),
        (
            "done",
            Json::arr(r.ok.iter().map(|s| Json::s(s.clone())).collect()),
        ),
        (
            "failed",
            Json::arr(r.failed.iter().map(|s| Json::s(s.clone())).collect()),
        ),
    ])
}

fn fail(e: &str) -> i32 {
    print_json(&Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::s(e))]));
    1
}

fn usage_fail(e: &str) -> i32 {
    eprintln!("参数错误: {e}（用 help 查看用法）");
    print_json(&Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::s(e))]));
    2
}
