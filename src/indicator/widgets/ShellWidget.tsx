import { useEffect, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { listen } from "@tauri-apps/api/event"
import type { ShellWidgetConfig } from "../types"

interface Props {
  fontFamily: string
  widgetName: string
}

export function ShellWidget({ fontFamily, widgetName }: Props) {
  const [output, setOutput] = useState("-")
  const [config, setConfig] = useState<ShellWidgetConfig | null>(null)

  // Load config from settings
  useEffect(() => {
    invoke<{ shell_widgets: ShellWidgetConfig[] }>("get_settings").then((s) => {
      const found = s.shell_widgets.find((w) => w.name === widgetName)
      if (found) setConfig(found)
    })

    const unlisten = listen<{ shell_widgets: ShellWidgetConfig[] }>(
      "settings-changed",
      (event) => {
        const found = event.payload.shell_widgets.find((w) => w.name === widgetName)
        if (found) setConfig(found)
      },
    )

    return () => {
      unlisten.then((fn) => fn())
    }
  }, [widgetName])

  // Poll the shell script
  useEffect(() => {
    if (!config) return

    const run = async () => {
      try {
        const result = await invoke<string>("run_shell_widget", {
          script: config.script ?? null,
          scriptPath: config.script_path ?? null,
        })
        setOutput(result || "-")
      } catch {
        setOutput("err")
      }
    }

    run()
    const intervalMs = (config.interval_secs || 10) * 1000
    const id = setInterval(run, intervalMs)

    return () => clearInterval(id)
  }, [config])

  return (
    <div
      style={{
        fontSize: "9px",
        opacity: 0.9,
        fontFamily,
        whiteSpace: "nowrap",
        paddingTop: 2,
        overflow: "hidden",
        textOverflow: "ellipsis",
        maxWidth: "100%",
      }}
    >
      {output}
    </div>
  )
}
