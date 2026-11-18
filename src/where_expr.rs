//! `--where`: a JavaScript expression over the event `e`, run in an embedded QuickJS engine.

use rquickjs::{CatchResultExt, Coerced, Context, Ctx, Function, Persistent, Runtime, Value};

use crate::watcher::WatchEvent;

pub struct WhereExpr {
    // Field order matters: the function must drop before the context and runtime.
    func: Persistent<Function<'static>>,
    ctx: Context,
    _rt: Runtime,
    pub source: String,
}

fn caught_message(e: rquickjs::CaughtError<'_>) -> String {
    match e {
        rquickjs::CaughtError::Exception(ex) => {
            let name = ex.get::<_, String>("name").unwrap_or_else(|_| "Error".into());
            let msg = ex.message().unwrap_or_default();
            if msg.is_empty() {
                name
            } else {
                format!("{name}: {msg}")
            }
        }
        rquickjs::CaughtError::Value(v) => format!("{v:?}"),
        rquickjs::CaughtError::Error(err) => err.to_string(),
    }
}

impl WhereExpr {
    /// Compile `source`. A syntax error is returned as a message.
    pub fn compile(source: &str) -> Result<WhereExpr, String> {
        let rt = Runtime::new().map_err(|e| e.to_string())?;
        let ctx = Context::full(&rt).map_err(|e| e.to_string())?;
        let code = format!("(function (e) {{ return ({source}\n); }})");
        let func = ctx.with(|ctx: Ctx<'_>| -> Result<_, String> {
            let f: Function = ctx.eval(code).catch(&ctx).map_err(caught_message)?;
            Ok(Persistent::save(&ctx, f))
        })?;
        Ok(WhereExpr { func, ctx, _rt: rt, source: source.to_string() })
    }

    /// Run the expression on one event. Returns its JavaScript truthiness, or the error message.
    pub fn test(&self, e: &WatchEvent) -> Result<bool, String> {
        let mut json = serde_json::to_value(e).map_err(|err| err.to_string())?;
        // Keep the documented `e.tool === undefined` for events that are not tool calls,
        // and expose the sort key like the original implementation did.
        if let serde_json::Value::Object(m) = &mut json {
            m.insert("ordinal".into(), e.ordinal.into());
        }
        let json = json.to_string();
        self.ctx.with(|ctx| {
            let f = self.func.clone().restore(&ctx).map_err(|err| err.to_string())?;
            let arg = ctx.json_parse(json).catch(&ctx).map_err(caught_message)?;
            let r: Value = f.call((arg,)).catch(&ctx).map_err(caught_message)?;
            let b: Coerced<bool> = r.get().catch(&ctx).map_err(caught_message)?;
            Ok(b.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::{Kind, Tool};

    fn ev(kind: Kind, text: &str) -> WatchEvent {
        WatchEvent {
            kind,
            thread_id: "t".into(),
            run_id: None,
            run: Some(3),
            item_id: None,
            item_type: Some("command_execution".into()),
            status: Some("failed".into()),
            at: "2026-11-18T17:00:00.000Z".into(),
            text: text.into(),
            tool: (kind == Kind::Tool).then(|| Tool { name: "shell".into(), input: "ls".into() }),
            intent: None,
            provider: None,
            ordinal: 1,
        }
    }

    #[test]
    fn evaluates_expressions() {
        let w = WhereExpr::compile(r#"e.kind !== "tool" || e.tool.name === "shell""#).unwrap();
        assert!(w.test(&ev(Kind::Tool, "shell: ls")).unwrap());
        assert!(w.test(&ev(Kind::User, "hi")).unwrap());
        let w = WhereExpr::compile("e.text.length > 3 && e.run === 3").unwrap();
        assert!(!w.test(&ev(Kind::User, "hi")).unwrap());
        assert!(w.test(&ev(Kind::User, "hello")).unwrap());
    }

    #[test]
    fn reports_errors() {
        assert!(WhereExpr::compile("e.kind ===").is_err());
        let w = WhereExpr::compile("e.tool.name").unwrap();
        let err = w.test(&ev(Kind::User, "hi")).unwrap_err();
        assert!(err.contains("TypeError"), "{err}");
    }
}
