use super::*;
impl RpcStream<'_> {
    pub(super) async fn dispatch_media(&mut self, cmd: Command) -> Result<(), CoreError> {
        match cmd {
            Command::ShotMonitors => self.on_shot_monitors().await,
            Command::ShotScreen(args) => self.on_shot_screen(args).await,
            Command::ShotWindow(args) => self.on_shot_window(args).await,
            Command::InputClick(args) => self.on_input_click(args).await,
            Command::InputKey(args) => self.on_input_key(args).await,
            Command::PowerExec(args) => self.on_power_exec(args).await,
            Command::PowerCancel => self.on_power_cancel().await,
            Command::PowerWakeWol(args) => self.on_power_wake_wol(args).await,
            Command::ClipGet => self.on_clip_get().await,
            Command::ClipSet(args) => self.on_clip_set(args).await,
            Command::TermOpen(args) => self.on_term_open(args).await,
            other => Err(CoreError::Internal(format!(
                "未分发的指令 {}",
                other.cmd_name()
            ))),
        }
    }
    async fn on_shot_monitors(&mut self) -> Result<(), CoreError> {
        let res = handle_shot_monitors(&self.node.platform, &self.record.permissions).await;
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_shot_screen(&mut self, args: ShotScreenArgs) -> Result<(), CoreError> {
        let res = handle_shot_screen_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            self.send,
        )
        .await;
        self.audit("shot.screen", "全屏截图", matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_shot_window(&mut self, args: ShotWindowArgs) -> Result<(), CoreError> {
        let detail = format!("HWND {}", args.hwnd);
        let res = handle_shot_window_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            self.send,
        )
        .await;
        self.audit("shot.window", &detail, matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_input_click(&mut self, args: InputClickArgs) -> Result<(), CoreError> {
        let detail = format!("({}, {})", args.screen_x, args.screen_y);
        let res = handle_input_click_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            self.send,
        )
        .await;
        self.audit("input.click", &detail, matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_input_key(&mut self, args: InputKeyArgs) -> Result<(), CoreError> {
        let detail = args
            .key_combo
            .clone()
            .unwrap_or_else(|| "文本输入".to_string());
        let res = handle_input_key_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            self.send,
        )
        .await;
        self.audit("input.key", &detail, matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_power_exec(&mut self, args: PowerExecArgs) -> Result<(), CoreError> {
        let detail = format!("{:?}", args.action);
        let res = self
            .node
            .handle_power_exec(&self.record.permissions, args)
            .await;
        self.audit("power.exec", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_power_cancel(&mut self) -> Result<(), CoreError> {
        let res = self.node.handle_power_cancel(&self.record.permissions);
        self.audit("power.cancel", "", res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_power_wake_wol(&mut self, args: PowerWakeWolArgs) -> Result<(), CoreError> {
        let detail = args.mac_addresses.join(", ");
        let res = self.node.handle_power_wake_wol(args);
        self.audit("power.wake_wol", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_clip_get(&mut self) -> Result<(), CoreError> {
        let res =
            handle_clip_get_stream(&self.node.platform, &self.record.permissions, self.send).await;
        self.audit("clip.get", "", matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_clip_set(&mut self, args: ClipSetArgs) -> Result<(), CoreError> {
        let detail = format!("{:?}", args.kind);
        let node_ref = self.node;
        let recv = self
            .recv
            .as_mut()
            .ok_or_else(|| CoreError::Internal("剪贴板接收流不可用".to_string()))?;
        let res = handle_clip_set_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            recv,
            self.send,
            |seq| node_ref.record_applied_clipboard_seq(seq),
        )
        .await;
        self.audit("clip.set", &detail, matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_term_open(&mut self, args: TermOpenArgs) -> Result<(), CoreError> {
        let detail = format!("{:?}", args.shell);
        let node_ref = self.node;
        let peer = self.peer;
        let peer_name = &self.record.name;
        let recv = self
            .recv
            .take()
            .ok_or_else(|| CoreError::Internal("终端输入流不可用".to_string()))?;
        let res = handle_term_open_stream(
            &self.node.platform,
            &self.record.permissions,
            args,
            self.send,
            recv,
            |ok| node_ref.record_audit(peer, peer_name, "term.open", &detail, ok),
        )
        .await;
        res?;
        Ok(())
    }
}
