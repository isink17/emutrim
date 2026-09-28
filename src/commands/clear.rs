use crate::managed::{self, ClearOps, ClearPlan, Layout, SystemClearOps};
use std::io::{self, BufRead, IsTerminal, Write};

pub fn run(args: Vec<String>) -> io::Result<()> {
    let (requested, yes) = parse_args(&args)?;
    let layout = Layout::resolve()?;
    if requires_terminal(
        requested.is_none(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    ) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: emutrim clear <AVD_NAME|all> [--yes]; no target requires interactive stdin and stdout"));
    }
    let plan = if requested == Some("all") {
        managed::plan_clear(&layout, None)?
    } else {
        managed::plan_clear(&layout, requested)?
    };
    let plan = if requested.is_none() {
        let name = select_target(
            plan.targets().iter().map(|target| target.name()).collect(),
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        )?;
        let Some(name) = name else {
            println!("clear cancelled");
            return Ok(());
        };
        plan.only(&name)
    } else {
        plan
    };

    if !plan.targets().is_empty() {
        plan.check_running(&SystemClearOps)?;
    }
    print_plan(&plan);
    if plan.targets().is_empty() {
        return Ok(());
    }
    if requested.is_none()
        && !yes
        && !confirm(
            &plan.targets()[0].name(),
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        )?
    {
        println!("clear cancelled");
        return Ok(());
    }
    if !yes {
        println!("dry run; pass --yes to delete these managed AVD resources");
        return Ok(());
    }
    execute(plan, &SystemClearOps)
}

fn requires_terminal(interactive_form: bool, stdin_terminal: bool, stdout_terminal: bool) -> bool {
    interactive_form && !(stdin_terminal && stdout_terminal)
}

fn execute(plan: ClearPlan, ops: &impl ClearOps) -> io::Result<()> {
    plan.check_running(ops)?;
    plan.execute_with(ops)?;
    println!("managed AVD resources removed");
    Ok(())
}

fn parse_args(args: &[String]) -> io::Result<(Option<&str>, bool)> {
    let (target, yes) = match args {
        [] => (None, false),
        [yes] if yes == "--yes" => (None, true),
        [target] => (Some(target.as_str()), false),
        [target, yes] if yes == "--yes" => (Some(target.as_str()), true),
        _ => return Err(usage()),
    };
    if target.is_some_and(|name| name.starts_with('-')) {
        return Err(usage());
    }
    Ok((target, yes))
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: emutrim clear [AVD_NAME|all] [--yes]",
    )
}

fn print_plan(plan: &ClearPlan) {
    if plan.targets().is_empty() {
        println!("no managed AVDs to clear");
    }
    for target in plan.targets() {
        println!("managed AVD {:?}:", target.name());
        println!("  {}", target.avd_dir().display());
        println!("  {}", target.ini().display());
    }
}

fn select_target<R: BufRead, W: Write>(
    names: Vec<String>,
    input: &mut R,
    output: &mut W,
) -> io::Result<Option<String>> {
    if names.is_empty() {
        writeln!(output, "no managed AVDs")?;
        return Ok(None);
    }
    for (index, name) in names.iter().enumerate() {
        writeln!(output, "{}. {name}", index + 1)?;
    }
    write!(output, "Select one managed AVD: ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(None);
    }
    let Ok(index) = answer.trim().parse::<usize>() else {
        return Ok(None);
    };
    if index == 0 || index > names.len() {
        return Ok(None);
    }
    Ok(Some(names[index - 1].clone()))
}

fn confirm<R: BufRead, W: Write>(name: &str, input: &mut R, output: &mut W) -> io::Result<bool> {
    write!(output, "Delete managed AVD {name:?}? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn selector_requires_exact_valid_number() {
        for bad in ["0", "-1", "3", "x", "", "\n"] {
            assert_eq!(
                select_target(
                    vec!["one".into(), "two".into()],
                    &mut Cursor::new(bad),
                    &mut Vec::new()
                )
                .unwrap(),
                None
            );
        }
        assert_eq!(
            select_target(
                vec!["one".into(), "two".into()],
                &mut Cursor::new("2\n"),
                &mut Vec::new()
            )
            .unwrap(),
            Some("two".into())
        );
        assert_eq!(
            select_target(Vec::new(), &mut Cursor::new("1\n"), &mut Vec::new()).unwrap(),
            None
        );
        assert_eq!(
            select_target(
                vec!["one".into()],
                &mut Cursor::new(Vec::<u8>::new()),
                &mut Vec::new()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn confirmation_accepts_only_explicit_yes() {
        for answer in ["", "n\n", "no\n", "wat\n"] {
            assert!(!confirm("x", &mut Cursor::new(answer), &mut Vec::new()).unwrap());
        }
        assert!(confirm("x", &mut Cursor::new("y\n"), &mut Vec::new()).unwrap());
        assert!(confirm("x", &mut Cursor::new("yes\n"), &mut Vec::new()).unwrap());
        assert!(!confirm("x", &mut Cursor::new(Vec::<u8>::new()), &mut Vec::new()).unwrap());
    }

    #[test]
    fn targetless_clear_requires_both_terminals() {
        assert!(requires_terminal(true, false, true));
        assert!(requires_terminal(true, true, false));
        assert!(!requires_terminal(true, true, true));
        assert!(!requires_terminal(false, false, false));
    }
}
