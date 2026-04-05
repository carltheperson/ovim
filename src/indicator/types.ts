export type VimMode = "insert" | "normal" | "visual"

export type WidgetType =
  | "None"
  | "Time"
  | "Date"
  | "CharacterCount"
  | "LineCount"
  | "CharacterAndLineCount"
  | "Battery"
  | "CapsLock"
  | "KeystrokeBuffer"
  | `Shell:${string}`

export interface RgbColor {
  r: number
  g: number
  b: number
}

export interface ModeColors {
  insert: RgbColor
  normal: RgbColor
  visual: RgbColor
}

export interface ShellWidgetConfig {
  name: string
  script?: string
  script_path?: string
  interval_secs: number
}

export type RowItem =
  | { type: "ModeChar"; size: 1 | 2 | 3 }
  | { type: "Widget"; widget_type: WidgetType }

export interface Settings {
  enabled: boolean
  indicator_position: number
  indicator_opacity: number
  indicator_size: number
  indicator_offset_x: number
  indicator_offset_y: number
  indicator_visible: boolean
  show_mode_in_menu_bar: boolean
  mode_colors: ModeColors
  indicator_font: string
  indicator_rows: RowItem[]
  shell_widgets: ShellWidgetConfig[]
}

export interface SelectionInfo {
  char_count: number
  line_count: number
}

export interface BatteryInfo {
  percentage: number
  is_charging: boolean
}
