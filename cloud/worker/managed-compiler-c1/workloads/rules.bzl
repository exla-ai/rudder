# Actual fixed Buck2 local C++ action. No remote cache/RE/credentials/network.
def _compile(ctx):
    output = ctx.actions.declare_output("c1-buck-cxx")
    ctx.actions.run(cmd_args("/usr/bin/g++", "-std=c++17", ctx.attrs.source, "-o", output.as_output()), category = "c1_cxx", local_only = True)
    return [DefaultInfo(default_output = output)]
c1_cxx = rule(impl = _compile, attrs = {"source": attrs.source()})
def _platforms(ctx):
    executor = CommandExecutorConfig(local_enabled = True, remote_enabled = False, remote_cache_enabled = False, allow_cache_uploads = False)
    platform = ExecutionPlatformInfo(label = ctx.label.raw_target(), configuration = ConfigurationInfo(constraints = {}, values = {}), executor_config = executor)
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = [platform])]
platforms = rule(impl = _platforms, attrs = {})
