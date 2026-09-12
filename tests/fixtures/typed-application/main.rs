#[allow(dead_code)]
mod v1 {
    include!(env!("UNIONID_TYPED_APP_V1"));
}

#[allow(dead_code)]
mod v2 {
    include!(env!("UNIONID_TYPED_APP_V2"));
}

use unionid::scalars::{Bytes, Date, Decimal, Duration, Timestamp};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database = std::env::var("UNIONID_TYPED_APP_DB")?;
    let schema = std::fs::read_to_string(std::env::var("UNIONID_TYPED_APP_SCHEMA")?)?;
    let migration = std::fs::read_to_string(std::env::var("UNIONID_TYPED_APP_MIGRATION")?)?;

    let first_at: Timestamp = "2026-09-13T08:00:00Z".parse()?;
    let second_at: Timestamp = "2026-09-13T09:00:00Z".parse()?;
    let due: Date = "2026-09-14".parse()?;
    let wait: Duration = "30seconds".parse()?;
    let payload: Bytes = "00ff10".parse()?;

    {
        let mut engine = unionid::Engine::open_redb(&database)?;
        ensure_ok(engine.execute(&schema))?;
        for task in [
            v1::Task {
                id: 1,
                title: "first".into(),
                state: v1::State::Running { attempt: 2 },
                note: None,
                price: Decimal::parse("10.25", 18, 2)?,
                created_at: first_at.clone(),
                due_on: due.clone(),
                wait: wait.clone(),
                payload: payload.clone(),
            },
            v1::Task {
                id: 2,
                title: "second".into(),
                state: v1::State::Failed {
                    message: "network".into(),
                    retry_at: Some(second_at.clone()),
                },
                note: Some(None),
                price: Decimal::parse("19.75", 18, 2)?,
                created_at: second_at.clone(),
                due_on: due.clone(),
                wait: wait.clone(),
                payload: payload.clone(),
            },
        ] {
            let inserted = v1::create_task::create_task(
                &mut engine,
                v1::create_task::CreateTaskParams { task },
            )?;
            assert_eq!(inserted.affected_rows, 1);
        }
        for event in [
            v1::Event { id: 10, task_id: 1, label: "started".into() },
            v1::Event { id: 11, task_id: 1, label: "retried".into() },
        ] {
            v1::add_event::add_event(&mut engine, v1::add_event::AddEventParams { event })?;
        }
    }

    {
        let mut engine = unionid::Engine::open_redb(&database)?;
        let first = v1::find_task::find_task(
            &mut engine,
            v1::find_task::FindTaskParams { id: 1 },
        )?
        .ok_or("missing first task")?;
        let second = v1::find_task::find_task(
            &mut engine,
            v1::find_task::FindTaskParams { id: 2 },
        )?
        .ok_or("missing second task")?;
        assert_eq!(first.note, None);
        assert_eq!(second.note, Some(None));
        assert_eq!(first.price, Decimal::parse("10.25", 18, 2)?);
        assert_eq!(first.created_at, first_at);
        assert_eq!(first.due_on, due);
        assert_eq!(first.wait, wait);
        assert_eq!(first.payload, payload);

        let classified = v1::classify::classify(&mut engine, v1::classify::ClassifyParams)?;
        assert_eq!(classified[0].title_size, 5);
        assert_eq!(classified[1].state_label, "failed");
        let summary = v1::summary::summary(&mut engine, v1::summary::SummaryParams)?;
        assert_eq!(summary.tasks, 2);
        assert_eq!(summary.total, Decimal::parse("30.00", 18, 2)?);
        assert_eq!(summary.earliest, Some(first_at.clone()));
        let related = v1::tasks_with_events::tasks_with_events(
            &mut engine,
            v1::tasks_with_events::TasksWithEventsParams,
        )?;
        assert_eq!(related[0].events.len(), 2);
        assert!(related[1].events.is_empty());

        ensure_ok(engine.execute(&migration))?;
        let old_read = v1::find_task::find_task(
            &mut engine,
            v1::find_task::FindTaskParams { id: 1 },
        )
        .unwrap_err();
        assert_eq!(old_read.code, "E_SCHEMA_CHANGED");
        let old_write = v1::create_task::create_task(
            &mut engine,
            v1::create_task::CreateTaskParams {
                task: v1::Task {
                    id: 3,
                    title: "stale".into(),
                    state: v1::State::Done,
                    note: None,
                    price: Decimal::parse("1.00", 18, 2)?,
                    created_at: second_at.clone(),
                    due_on: due.clone(),
                    wait: wait.clone(),
                    payload: payload.clone(),
                },
            },
        )
        .unwrap_err();
        assert_eq!(old_write.code, "E_SCHEMA_CHANGED");
    }

    {
        let mut engine = unionid::Engine::open_redb(&database)?;
        let migrated = v2::find_task::find_task(
            &mut engine,
            v2::find_task::FindTaskParams { id: 1 },
        )?
        .ok_or("missing migrated task")?;
        assert_eq!(migrated.priority, 0);
        let archived = v2::Task {
            id: 4,
            title: "archived".into(),
            state: v2::State::Archived { at: second_at },
            note: Some(Some("kept".into())),
            price: Decimal::parse("2.00", 18, 2)?,
            created_at: first_at,
            due_on: due,
            wait,
            payload,
            priority: 3,
        };
        v2::create_task::create_task(
            &mut engine,
            v2::create_task::CreateTaskParams { task: archived },
        )?;
        let classified = v2::classify::classify(&mut engine, v2::classify::ClassifyParams)?;
        assert_eq!(classified.last().unwrap().state_label, "archived");
        assert_eq!(classified.last().unwrap().priority, 3);
    }
    Ok(())
}

fn ensure_ok(response: unionid::QueryResponse) -> Result<(), unionid::Error> {
    if response.ok {
        Ok(())
    } else {
        Err(response.error.unwrap_or_else(|| unionid::Error::new("E_QUERY", response.message)))
    }
}
