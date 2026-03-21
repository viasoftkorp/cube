cube('masking_policy_split_test', {
  sql: `
    SELECT * FROM (VALUES
      (1, 'RESEARCH', 1, '100 Main St'),
      (2, 'DEMO', 1, '200 Main St'),
      (3, 'LATAM', 0, '300 Main St'),
      (4, 'APAC', 1, '400 Main St'),
      (5, 'EMEA', 1, '500 Main St'),
      (6, 'LATAM', 1, '600 Main St')
    ) AS src(id, data_security_field, region_lock_data, address_line_1)
  `,

  dimensions: {
    id: {
      sql: 'id',
      type: 'number',
      primaryKey: true,
    },

    data_security_field: {
      sql: 'data_security_field',
      type: 'string',
      public: false,
    },

    region_lock_data: {
      sql: 'region_lock_data',
      type: 'number',
      public: false,
    },

    address_line_1: {
      sql: 'address_line_1',
      type: 'string',
      mask: {
        sql: `
          CASE
            WHEN ${CUBE}.data_security_field IN ('RESEARCH', 'DEMO') THEN ${CUBE}.address_line_1
            WHEN ${SECURITY_CONTEXT.access_group.filter((accessGroup) => `${accessGroup} = 'Sensitive Data Access'`)}
                 AND ${CUBE}.region_lock_data = 0 THEN ${CUBE}.address_line_1
            WHEN ${SECURITY_CONTEXT.data_region.filter(`${CUBE}.data_security_field`)}
              THEN ${CUBE}.address_line_1
            ELSE '***MASKED***'
          END
        `,
      },
    },
  },

  accessPolicy: [
    {
      group: 'Sensitive Data Access',
      memberLevel: {
        excludes: ['address_line_1'],
      },
      memberMasking: {
        includes: ['address_line_1'],
      },
      rowLevel: {
        allowAll: true,
      },
    },
    {
      group: 'Very Sensitive Data Access',
      memberLevel: {
        excludes: ['address_line_1'],
      },
      memberMasking: {
        includes: ['address_line_1'],
      },
      rowLevel: {
        allowAll: true,
      },
    },
  ],
});
