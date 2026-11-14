-- NAME: a Thurm plugin. After editing: thurm plugin reload NAME
-- Editor completion for the thurm API: thurm plugin types

thurm.action{
  name = "NAME.hello",
  title = "NAME: Say hello",
  run = function(ctx)
    thurm.ui.toast("Hello from NAME", ctx.pane)
  end,
}

thurm.on("agent_finished", function(ev)
  thurm.log("agent finished in pane", ev.pane.id)
end)
