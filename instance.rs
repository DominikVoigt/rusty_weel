weel!().call(
    "a1",
    "bookAir",
    HTTPParams {
        label: "Book Airline 12",
        method: Method::POST,
        arguments: json!([
                {
                    "name": "from",
                    "value": r###"!data.from"###
                },
                {
                    "name": "to",
                    "value": r###"!data.to"###
                },
                {
                    "name": "persons",
                    "value": r###"!data.persons"###
                }
        ]),
    },
    json!({
                "_generic": null,
                "_logging_behavior": {
                    "_exclude": r###"false"###,
                    "_include": r###"false"###
                },
                "_timing": {
                    "_timing_weight": null,
                    "_timing_avg": null,
                    "explanations": null
                },
                "_shifting": {
                    "_shifting_type": r###"Duration"###
                },
                "_context_data_analysis": {
                    "probes": null,
                    "ips": null
                },
                "report": {
                    "url": null
                },
                "_notes": {
                    "_notes_general": null
                }
    }),
    None,
    None,
    code! {
    r###"
        data.airline = result.value('id')
        data.costs += result.value('costs').to_f
        status.update 1, 'Hotel'
    "###},
    None,
)?;
weel!().parallel_do(None,Last,ƛ!({
    weel!().loop_exec(Weel::pre_test("data.persons > 0"), ƛ!({
        weel!().parallel_branch(pƛ!({
            weel!().call(
                "a2",
                "bookHotel",
                HTTPParams {
                    label: "Book Hotel",
                    method: Method::POST,
                    arguments: json!([
                            {
                                "name": "to",
                                "value": r###"!data.to"###
                            }
                    ]),
                },
                json!({
                            "_timing": {
                                "_timing_weight": null,
                                "_timing_avg": null,
                                "explanations": null
                            },
                            "_shifting": {
                                "_shifting_type": r###"Duration"###
                            },
                            "_context_data_analysis": {
                                "probes": null,
                                "ips": null
                            },
                            "report": {
                                "url": null
                            },
                            "_notes": {
                                "_notes_general": null
                            }
                }),
                None,
                None,
                code! {
                r###"
                    data.hotels << result.value('id')
                    data.costs += result.value('costs').to_f
                "###},
                None,
            )?;
        }))?;
        weel!().manipulate(
            "a3",
            None,
            code! {
            r###"
                data.persons -= 1
            "###},
        )?;
    }))?;
}))?;
weel!().choose(Exclusive, ƛ!({
    weel!().alternative("data.costs > 700", ƛ!({
        weel!().call(
            "a4",
            "approve",
            HTTPParams {
                label: "Approve Hotel",
                method: Method::POST,
                arguments: json!([
                        {
                            "name": "costs",
                            "value": r###"!data.costs"###
                        }
                ]),
            },
            json!({
                        "_timing": {
                            "_timing_weight": null,
                            "_timing_avg": null,
                            "explanations": null
                        },
                        "_shifting": {
                            "_shifting_type": r###"Duration"###
                        },
                        "_context_data_analysis": {
                            "probes": null,
                            "ips": null
                        },
                        "report": {
                            "url": null
                        },
                        "_notes": {
                            "_notes_general": null
                        }
            }),
            None,
            None,
            None,
            None,
        )?;
    }))?;
    weel!().otherwise(ƛ!({
    }))?;
}))?;
