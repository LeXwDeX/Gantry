/**
 * 浮层集合：设置、审批单、收件箱、配对。
 * 只经 IpcAdapter 调用外壳；不读取 fixture，不伪造签名或保存成功。
 */
import "./overlays.css";

export { SettingsOverlay } from "./SettingsOverlay";
export type { SettingsOverlayProps } from "./SettingsOverlay";
export { ApprovalSheet } from "./ApprovalSheet";
export type { ApprovalSheetProps } from "./ApprovalSheet";
export { InboxOverlay } from "./InboxOverlay";
export type { InboxOverlayProps } from "./InboxOverlay";
export { PairingView } from "./PairingView";
export type { PairingViewProps } from "./PairingView";
