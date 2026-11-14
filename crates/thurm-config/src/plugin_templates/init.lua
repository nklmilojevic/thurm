-- __PLUGIN__: a Thurm plugin. After editing: thurm plugin reload __PLUGIN__
-- Editor completion for the thurm API: thurm plugin types

thurm.action{
  name = "__PLUGIN__.hello",
  title = "__PLUGIN__: Say hello",
  run = function(ctx)
    thurm.ui.toast("Hello from __PLUGIN__", ctx.pane)
  end,
}

thurm.on("agent_finished", function(ev)
  thurm.log("agent finished in pane", ev.pane.id)
end)
