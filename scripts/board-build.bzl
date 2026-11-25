def _board_platform(ctx):
    executor = CommandExecutorConfig(
        local_enabled = True,
        remote_enabled = False,
        remote_cache_enabled = True,
        allow_cache_uploads = True,
        remote_execution_properties = {"OSFamily": "linux", "Arch": "x86_64"},
        remote_execution_use_case = "rudder-board",
    )
    platform = ExecutionPlatformInfo(
        label = ctx.label.raw_target(),
        configuration = ConfigurationInfo(constraints = {}, values = {}),
        executor_config = executor,
    )
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]

board_platform = rule(impl = _board_platform, attrs = {})

def _board_stylesheet(ctx):
    # Bundle from the declared input alone. A new relative import must first
    # be added to this rule before it can affect the cached output.
    inputs = ctx.actions.copied_dir("style-inputs", {"board.css": ctx.attrs.css})
    output = ctx.actions.declare_output("board.css")
    ctx.actions.run(
        cmd_args(
            ctx.attrs.esbuild,
            inputs.project("board.css"),
            "--bundle",
            "--minify",
            cmd_args(output.as_output(), format = "--outfile={}"),
        ),
        category = "board_stylesheet",
        allow_cache_upload = True,
    )
    return [DefaultInfo(default_output = output)]

board_stylesheet = rule(
    impl = _board_stylesheet,
    attrs = {"css": attrs.source(), "esbuild": attrs.source()},
)
