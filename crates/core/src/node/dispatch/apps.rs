use super::*;
impl RpcStream<'_> {
    pub(super) async fn dispatch_apps(&mut self, cmd: Command) -> Result<(), CoreError> {
        match cmd {
            Command::AppList(args) => self.on_app_list(args).await,
            Command::AppIcon(args) => self.on_app_icon(args).await,
            Command::AppLaunch(args) => self.on_app_launch(args).await,
            Command::ProcList => self.on_proc_list().await,
            Command::ProcKill(args) => self.on_proc_kill(args).await,
            other => self.dispatch_media(other).await,
        }
    }
    async fn on_app_list(&mut self, args: AppListArgs) -> Result<(), CoreError> {
        let compress = self.supports_deflate();
        let res = handle_app_list(&self.node.platform, &self.record.permissions, args).await;
        write_rpc_result_opts(self.send, res, compress).await?;
        Ok(())
    }
    async fn on_app_icon(&mut self, args: AppIconArgs) -> Result<(), CoreError> {
        let res = handle_app_icon(&self.node.platform, &self.record.permissions, args).await;
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_app_launch(&mut self, args: AppLaunchArgs) -> Result<(), CoreError> {
        let detail = args.target.clone();
        let res = handle_app_launch(&self.node.platform, &self.record.permissions, args).await;
        self.audit("app.launch", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_proc_list(&mut self) -> Result<(), CoreError> {
        let compress = self.supports_deflate();
        let res = handle_proc_list(&self.node.platform, &self.record.permissions).await;
        write_rpc_result_opts(self.send, res, compress).await?;
        Ok(())
    }
    async fn on_proc_kill(&mut self, args: ProcKillArgs) -> Result<(), CoreError> {
        let detail = format!("PID {}", args.pid);
        let res = handle_proc_kill(&self.node.platform, &self.record.permissions, args).await;
        self.audit("proc.kill", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
}
